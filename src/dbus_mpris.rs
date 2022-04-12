use chrono::prelude::*;
use chrono::Duration;
use dbus::arg::{RefArg, Variant};
use dbus::channel::{MatchingReceiver, Sender};
use dbus::message::{MatchRule, SignalArgs};
use dbus::MethodErr;
use dbus_crossroads::{Crossroads, IfaceToken};
use dbus_tokio::connection;
use futures::task::{Context, Poll};
use futures::{self, Future};
use librespot_connect::spirc::Spirc;
use librespot_core::keymaster::{get_token, Token as LibrespotToken};
use librespot_core::mercury::MercuryError;
use librespot_core::session::Session;
use librespot_playback::player::PlayerEvent;
use log::info;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use rspotify::model::offset::Offset;
use rspotify::model::track::FullTrack;
use rspotify::model::{
    AlbumId, ArtistId, EpisodeId, IdError, PlayableItem, PlaylistId, RepeatState, ShowId, TrackId,
    Type,
};
use rspotify::prelude::*;
use rspotify::{AuthCodeSpotify, Token as RspotifyToken};
use std::collections::HashMap;
use std::env;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub struct DbusServer {
    session: Session,
    spirc: Arc<Spirc>,
    spotify_client: Arc<AuthCodeSpotify>,
    #[allow(clippy::type_complexity)]
    token_request: Option<Pin<Box<dyn Future<Output = Result<LibrespotToken, MercuryError>>>>>,
    dbus_future: Option<Pin<Box<dyn Future<Output = ()>>>>,
    device_name: String,
    event_rx: UnboundedReceiver<PlayerEvent>,
    event_tx: Option<UnboundedSender<PlayerEvent>>,
}

const CLIENT_ID: &str = "2c1ea588dfbc4a989e2426f8385297c3";
const SCOPE: &str = "user-read-playback-state,user-read-private,\
                     user-read-email,playlist-read-private,user-library-read,user-library-modify,\
                     user-top-read,playlist-read-collaborative,playlist-modify-public,\
                     playlist-modify-private,user-follow-read,user-follow-modify,\
                     user-read-currently-playing,user-modify-playback-state,\
                     user-read-recently-played";

impl DbusServer {
    pub fn new(
        session: Session,
        spirc: Arc<Spirc>,
        device_name: String,
        event_rx: UnboundedReceiver<PlayerEvent>,
    ) -> DbusServer {
        DbusServer {
            session,
            spirc,
            spotify_client: Default::default(),
            token_request: None,
            dbus_future: None,
            device_name,
            event_rx,
            event_tx: None,
        }
    }
}

impl Future for DbusServer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.event_tx.is_some() {
            if let Poll::Ready(Some(msg)) = self.event_rx.poll_recv(cx) {
                self.event_tx.as_ref().unwrap().send(msg).unwrap();
            }
        }
        let needs_token = match *self.spotify_client.get_token().lock().unwrap() {
            Some(ref token) => token.is_expired(),
            None => true,
        };

        if needs_token {
            if let Some(mut fut) = self.token_request.take() {
                if let Poll::Ready(Ok(token)) = fut.as_mut().poll(cx) {
                    let expires_in = Duration::seconds(token.expires_in as i64);
                    let api_token = RspotifyToken {
                        access_token: token.access_token,
                        expires_in,
                        expires_at: Some(Utc::now() + expires_in),
                        ..RspotifyToken::default()
                    };

                    if self.dbus_future.is_none() {
                        self.spotify_client = Arc::new(AuthCodeSpotify::from_token(api_token));

                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        self.event_tx = Some(tx);
                        self.dbus_future = Some(Box::pin(create_dbus_server(
                            Arc::clone(&self.spotify_client),
                            self.spirc.clone(),
                            self.device_name.clone(),
                            rx,
                        )));
                    } else {
                        *self.spotify_client.get_token().lock().unwrap() = Some(api_token);
                    }

                    // TODO: for reasons I don't _entirely_ understand, the token request completing
                    // convinces callers that they don't need to re-check the status of this future
                    // until we start playing. This causes DBUS to not respond until that point in
                    // time. So, fire a "wake" here, which tells callers to keep checking.
                    cx.waker().clone().wake();
                } else {
                    self.token_request = Some(fut);
                }
            } else {
                self.token_request = Some(Box::pin({
                    let sess = self.session.clone();
                    // This is more meant as a fast hotfix than anything else!
                    let client_id =
                        env::var("SPOTIFYD_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.to_string());
                    async move { get_token(&sess, &client_id, SCOPE).await }
                }));
            }
        }

        if let Some(ref mut fut) = self.dbus_future {
            return fut.as_mut().poll(cx);
        }

        Poll::Pending
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

async fn create_dbus_server(
    spotify_api_client: Arc<AuthCodeSpotify>,
    spirc: Arc<Spirc>,
    device_name: String,
    mut event_rx: UnboundedReceiver<PlayerEvent>,
) {
    // TODO: allow other DBus types through CLI and config entry.
    let (resource, conn) =
        connection::new_session_sync().expect("Failed to initialize DBus connection");
    tokio::spawn(async {
        let err = resource.await;
        panic!("Lost connection to D-Bus: {}", err);
    });

    conn.request_name("org.mpris.MediaPlayer2.spotifyd", false, true, true)
        .await
        .expect("Failed to register dbus player name");

    let mut cr = Crossroads::new();
    cr.set_async_support(Some((
        conn.clone(),
        Box::new(|x| {
            tokio::spawn(x);
        }),
    )));

    // The following methods and properties are part of the MediaPlayer2 interface.
    // https://specifications.freedesktop.org/mpris-spec/latest/Media_Player.html
    let media_player2_interface = cr.register("org.mpris.MediaPlayer2", |b| {
        b.method("Raise", (), (), move |_, _, (): ()| {
            // noop
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("Quit", (), (), move |_, _, (): ()| {
            local_spirc.shutdown();
            Ok(())
        });
        b.property("CanQuit")
            .emits_changed_const()
            .get(|_, _| Ok(true));
        b.property("CanRaise")
            .emits_changed_const()
            .get(|_, _| Ok(false));
        b.property("CanSetFullscreen")
            .emits_changed_const()
            .get(|_, _| Ok(false));
        b.property("HasTrackList")
            .emits_changed_const()
            .get(|_, _| Ok(false));
        b.property("Identity")
            .emits_changed_const()
            .get(|_, _| Ok("Spotifyd".to_string()));
        b.property("SupportedUriSchemes")
            .emits_changed_const()
            .get(|_, _| Ok(vec!["spotify".to_string()]));
        b.property("SupportedMimeTypes")
            .emits_changed_const()
            .get(|_, _| Ok(Vec::<String>::new()));
    });

    // The following methods and properties are part of the MediaPlayer2.Player interface.
    // https://specifications.freedesktop.org/mpris-spec/latest/Player_Interface.html

    let player_interface: IfaceToken<()> = cr.register("org.mpris.MediaPlayer2.Player", |b| {
        let local_spirc = spirc.clone();
        b.method("VolumeUp", (), (), move |_, _, (): ()| {
            local_spirc.volume_up();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("VolumeDown", (), (), move |_, _, (): ()| {
            local_spirc.volume_down();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("Next", (), (), move |_, _, (): ()| {
            local_spirc.next();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("Previous", (), (), move |_, _, (): ()| {
            local_spirc.prev();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("Pause", (), (), move |_, _, (): ()| {
            local_spirc.pause();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("PlayPause", (), (), move |_, _, (): ()| {
            local_spirc.play_pause();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("Play", (), (), move |_, _, (): ()| {
            local_spirc.play();
            Ok(())
        });
        let local_spirc = spirc.clone();
        b.method("Stop", (), (), move |_, _, (): ()| {
            // TODO: add real stop implementation.
            local_spirc.pause();
            Ok(())
        });

        let mv_device_name = device_name.clone();
        let sp_client = Arc::clone(&spotify_api_client);
        b.method("Seek", ("pos",), (), move |_, _, (pos,): (u32,)| {
            let device_name = utf8_percent_encode(&mv_device_name, NON_ALPHANUMERIC).to_string();
            if let Ok(Some(playing)) = sp_client.current_user_playing_item() {
                let _ = sp_client.seek_track(
                    playing.progress.map(|d| d.as_millis()).unwrap_or(0) as u32 + pos,
                    Some(&device_name),
                );
            }
            Ok(())
        });

        let mv_device_name = device_name.clone();
        let sp_client = Arc::clone(&spotify_api_client);
        b.method("SetPosition", ("pos",), (), move |_, _, (pos,): (u32,)| {
            let device_name = utf8_percent_encode(&mv_device_name, NON_ALPHANUMERIC).to_string();
            let _ = sp_client.seek_track(pos, Some(&device_name));
            Ok(())
        });

        let mv_device_name = device_name.clone();
        let sp_client = Arc::clone(&spotify_api_client);
        b.method("OpenUri", ("uri",), (), move |_, _, (uri,): (String,)| {
            struct AnyContextId(Box<dyn PlayContextId>);

            impl Id for AnyContextId {
                fn id(&self) -> &str {
                    self.0.id()
                }

                fn _type(&self) -> Type {
                    self.0._type()
                }

                fn _type_static() -> Type
                where
                    Self: Sized,
                {
                    unreachable!("never called");
                }

                unsafe fn from_id_unchecked(_id: &str) -> Self
                where
                    Self: Sized,
                {
                    unreachable!("never called");
                }
            }
            impl PlayContextId for AnyContextId {}

            enum Uri {
                Playable(Box<dyn PlayableId>),
                Context(AnyContextId),
            }

            impl Uri {
                fn from_id(id_type: Type, id: &str) -> Result<Uri, IdError> {
                    use Uri::*;
                    let uri = match id_type {
                        Type::Track => Playable(Box::new(TrackId::from_id(id)?)),
                        Type::Episode => Playable(Box::new(EpisodeId::from_id(id)?)),
                        Type::Artist => Context(AnyContextId(Box::new(ArtistId::from_id(id)?))),
                        Type::Album => Context(AnyContextId(Box::new(AlbumId::from_id(id)?))),
                        Type::Playlist => Context(AnyContextId(Box::new(PlaylistId::from_id(id)?))),
                        Type::Show => Context(AnyContextId(Box::new(ShowId::from_id(id)?))),
                        Type::User | Type::Collection => Err(IdError::InvalidType)?,
                    };
                    Ok(uri)
                }
            }

            let mut chars = uri
                .strip_prefix("spotify")
                .ok_or(MethodErr::invalid_arg(&uri))?
                .chars();

            let sep = match chars.next() {
                Some(ch) if ch == '/' || ch == ':' => ch,
                _ => return Err(MethodErr::invalid_arg(&uri)),
            };
            let rest = chars.as_str();

            let (id_type, id) = rest
                .rsplit_once(sep)
                .and_then(|(id_type, id)| Some((id_type.parse::<Type>().ok()?, id)))
                .ok_or(MethodErr::invalid_arg(&uri))?;

            let uri = Uri::from_id(id_type, id).map_err(|_| MethodErr::invalid_arg(&uri))?;

            let device_name = utf8_percent_encode(&mv_device_name, NON_ALPHANUMERIC).to_string();
            let device_id = sp_client.device().ok().and_then(|devices| {
                devices.into_iter().find_map(|d| {
                    if d.is_active && d.name == device_name {
                        d.id
                    } else {
                        None
                    }
                })
            });

            match uri {
                Uri::Playable(id) => {
                    let _ = sp_client.start_uris_playback(
                        Some(id.as_ref()),
                        device_id.as_deref(),
                        Some(Offset::for_position(0)),
                        None,
                    );
                }
                Uri::Context(id) => {
                    let _ = sp_client.start_context_playback(
                        &id,
                        device_id.as_deref(),
                        Some(Offset::for_position(0)),
                        None,
                    );
                }
            }
            Ok(())
        });

        let mv_device_name = device_name.clone();
        let sp_client = Arc::clone(&spotify_api_client);
        b.property("PlaybackStatus")
            .emits_changed_false()
            .get(move |_, _| {
                if let Ok(Some(playing_context)) = sp_client.current_playback(None, None::<Vec<_>>)
                {
                    if playing_context.device.name == mv_device_name {
                        if playing_context.is_playing {
                            return Ok("Playing".to_string());
                        } else {
                            return Ok("Paused".to_string());
                        }
                    }
                }
                Ok("Stopped".to_string())
            });

        let sp_client = Arc::clone(&spotify_api_client);
        b.property("Shuffle")
            .emits_changed_false()
            .get(move |_, _| {
                let shuffle_status = sp_client
                    .current_playback(None, None::<Vec<_>>)
                    .ok()
                    .flatten()
                    .map_or(false, |p| p.shuffle_state);
                Ok(shuffle_status)
            });

        b.property("Rate").emits_changed_const().get(|_, _| Ok(1.0));

        let sp_client = Arc::clone(&spotify_api_client);
        b.property("Volume").emits_changed_false().get(move |_, _| {
            let vol = sp_client
                .current_playback(None, None::<Vec<_>>)
                .ok()
                .flatten()
                .and_then(|p| p.device.volume_percent)
                .unwrap_or(0) as f64;

            Ok(vol)
        });

        b.property("MaximumRate")
            .emits_changed_const()
            .get(|_, _| Ok(1.0));
        b.property("MinimumRate")
            .emits_changed_const()
            .get(|_, _| Ok(1.0));

        let sp_client = Arc::clone(&spotify_api_client);
        b.property("LoopStatus")
            .emits_changed_false()
            .get(move |_, _| {
                let status =
                    if let Ok(Some(player)) = sp_client.current_playback(None, None::<Vec<_>>) {
                        match player.repeat_state {
                            RepeatState::Off => "None",
                            RepeatState::Track => "Track",
                            RepeatState::Context => "Playlist",
                        }
                    } else {
                        "None"
                    }
                    .to_string();
                Ok(status)
            });

        let sp_client = Arc::clone(&spotify_api_client);
        b.property("Position")
            .emits_changed_false()
            .get(move |_, _| {
                let pos = sp_client
                    .current_playback(None, None::<Vec<_>>)
                    .ok()
                    .flatten()
                    .and_then(|p| Some(p.progress?.as_nanos() as i64))
                    .unwrap_or(0);

                Ok(pos)
            });

        let sp_client = Arc::clone(&spotify_api_client);
        b.property("Metadata")
            .emits_changed_false()
            .get(move |_, _| {
                let mut m: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
                let v = sp_client
                    .current_playing(None, None::<Vec<_>>)
                    .ok()
                    .flatten();

                if let Some(playing) = v {
                    match playing.item {
                        Some(PlayableItem::Track(track)) => insert_metadata(&mut m, track),
                        Some(PlayableItem::Episode(_episode)) => todo!(),
                        None => (),
                    }
                } else {
                    info!("Couldn't fetch metadata from spotify: {:?}", v);
                }

                Ok(m)
            });

        for prop in &[
            "CanPlay",
            "CanPause",
            "CanSeek",
            "CanControl",
            "CanGoPrevious",
            "CanGoNext",
        ] {
            b.property(*prop).emits_changed_const().get(|_, _| Ok(true));
        }
    });

    cr.insert(
        "/org/mpris/MediaPlayer2",
        &[media_player2_interface, player_interface],
        (),
    );

    conn.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |msg, conn| {
            cr.handle_message(msg, conn).unwrap();
            true
        }),
    );

    // Store current playback state to be able to detect changes
    let mut last_track_id = None;
    let mut last_playback_status = None;
    let mut last_volume = None;

    loop {
        let event = event_rx
            .recv()
            .await
            .expect("Changed track channel was unexpectedly closed");
        let mut seeked_position = None;

        // Update playback state from event
        let (track_id, playback_status, player_volume) = match event {
            PlayerEvent::VolumeSet { volume } => {
                (last_track_id, last_playback_status, Some(volume))
            }
            PlayerEvent::Playing {
                track_id,
                position_ms,
                ..
            } => {
                seeked_position = Some(position_ms);
                (Some(track_id), Some(PlaybackStatus::Playing), last_volume)
            }
            PlayerEvent::Stopped { .. } => {
                (last_track_id, Some(PlaybackStatus::Stopped), last_volume)
            }
            PlayerEvent::Paused { .. } => {
                (last_track_id, Some(PlaybackStatus::Paused), last_volume)
            }
            _ => continue,
        };

        // if playback_status, track_id or volume have changed, emit a PropertiesChanged signal
        if last_playback_status != playback_status
            || last_track_id != track_id
            || last_volume != player_volume
        {
            let mut changed_properties: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();

            if last_volume != player_volume {
                if let Some(player_volume) = player_volume {
                    // convert u16 to float
                    let mut vol_mpris = player_volume as f64;
                    // max. vol = 1.0 according to mpris spec, round to two decimal places
                    vol_mpris = (vol_mpris / 65535.0 * 100.0).round() / 100.0;
                    changed_properties
                        .insert("Volume".to_owned(), Variant(Box::new(vol_mpris.to_owned())));
                }
            } else {
                if let Some(track_id) = track_id {
                    let track_id = TrackId::from_id(&track_id.to_base62()).unwrap();
                    let track = spotify_api_client.track(&track_id);
                    if let Ok(track) = track {
                        let mut m: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
                        insert_metadata(&mut m, track);

                        changed_properties.insert("Metadata".to_owned(), Variant(Box::new(m)));
                    } else {
                        info!("Couldn't fetch metadata from spotify: {:?}", track);
                    }
                }
                if let Some(playback_status) = playback_status {
                    changed_properties.insert(
                        "PlaybackStatus".to_owned(),
                        Variant(Box::new(match playback_status {
                            PlaybackStatus::Playing => "Playing".to_owned(),
                            PlaybackStatus::Paused => "Paused".to_owned(),
                            PlaybackStatus::Stopped => "Stopped".to_owned(),
                        })),
                    );
                }
            }

            let msg = dbus::nonblock::stdintf::org_freedesktop_dbus::PropertiesPropertiesChanged {
                interface_name: "org.mpris.MediaPlayer2.Player".to_owned(),
                changed_properties,
                invalidated_properties: Vec::new(),
            };
            conn.send(msg.to_emit_message(&dbus::Path::new("/org/mpris/MediaPlayer2").unwrap()))
                .unwrap();

            last_playback_status = playback_status;
            last_track_id = track_id;
            last_volume = player_volume;
        }

        // if position in track has changed emit a Seeked signal
        if let Some(position) = seeked_position {
            let msg = dbus::message::Message::signal(
                &dbus::Path::new("/org/mpris/MediaPlayer2").unwrap(),
                &dbus::strings::Interface::new("org.mpris.MediaPlayer2.Player").unwrap(),
                &dbus::strings::Member::new("Seeked").unwrap(),
            )
            .append1(position as i64);
            conn.send(msg).unwrap();
        }
    }
}

fn insert_metadata(m: &mut HashMap<String, Variant<Box<dyn RefArg>>>, track: FullTrack) {
    m.insert(
        "mpris:trackid".to_string(),
        Variant(Box::new(track.id.map(|t| t.uri()).unwrap_or_default())),
    );

    m.insert(
        "mpris:length".to_string(),
        Variant(Box::new(track.duration.as_micros() as i64)),
    );

    m.insert(
        "mpris:artUrl".to_string(),
        Variant(Box::new(
            track
                .album
                .images
                .into_iter()
                .next()
                .map(|i| i.url)
                .unwrap_or_default(),
        )),
    );

    m.insert("xesam:title".to_string(), Variant(Box::new(track.name)));

    m.insert(
        "xesam:album".to_string(),
        Variant(Box::new(track.album.name)),
    );

    m.insert(
        "xesam:artist".to_string(),
        Variant(Box::new(
            track
                .artists
                .into_iter()
                .map(|a| a.name)
                .collect::<Vec<_>>(),
        )),
    );

    m.insert(
        "xesam:albumArtist".to_string(),
        Variant(Box::new(
            track
                .album
                .artists
                .into_iter()
                .map(|a| a.name)
                .collect::<Vec<_>>(),
        )),
    );

    m.insert(
        "xesam:autoRating".to_string(),
        Variant(Box::new(f64::from(track.popularity) / 100.0)),
    );

    m.insert(
        "xesam:trackNumber".to_string(),
        Variant(Box::new(track.track_number)),
    );

    m.insert(
        "xesam:discNumber".to_string(),
        Variant(Box::new(track.disc_number)),
    );

    let mut external_urls = track.external_urls;
    m.insert(
        "xesam:url".to_string(),
        Variant(Box::new(
            external_urls.remove("spotify").unwrap_or_default(),
        )),
    );
}
