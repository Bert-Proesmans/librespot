use futures::sync::oneshot;
use futures::{future, Future};
use std::borrow::Cow;
use std::mem;
use std::sync::mpsc::{RecvError, TryRecvError};
use std::thread;
use std;
use std::io::{Read, Error, ErrorKind, Seek, SeekFrom};

use core::config::{Bitrate, PlayerConfig};
use core::session::Session;
use core::util::{self, SpotifyId, Subfile};

use audio_backend::Sink;
use audio::{AudioFile, AudioDecrypt};
use audio::{VorbisDecoder, VorbisPacket};
use metadata::{FileFormat, Track, Metadata};
use mixer::AudioFilter;

#[derive(Clone)]
pub struct Player {
    session: Session,
    commands: std::sync::mpsc::Sender<PlayerCommand>,
}

struct PlayerInternal {
    session: Session,
    config: PlayerConfig,
    commands: std::sync::mpsc::Receiver<PlayerCommand>,

    state: PlayerState,
    sink: Box<Sink>,
    audio_filter: Option<Box<AudioFilter + Send>>,
}

enum PlayerCommand {
    Load(SpotifyId, bool, u32, oneshot::Sender<()>),
    Play,
    Pause,
    Stop,
    Seek(u32),
}

impl Player {
    pub fn new<F>(config: PlayerConfig, session: Session,
                  audio_filter: Option<Box<AudioFilter + Send>>,
                  sink_builder: F) -> Player
        where F: FnOnce() -> Box<Sink> + Send + 'static
    {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let session_internal = session.clone();

        thread::spawn(move || {
            debug!("new Player[{}]", session_internal.session_id());

            let internal = PlayerInternal {
                session: session_internal,
                config: config,
                commands: cmd_rx,

                state: PlayerState::Stopped,
                sink: sink_builder(),
                audio_filter: audio_filter,
            };

            internal.run();
        });

        Player {
            session: session,
            commands: cmd_tx,
        }
    }

    fn command(&self, cmd: PlayerCommand) {
        self.commands.send(cmd).unwrap();
    }

    pub fn load(&self, track: SpotifyId, start_playing: bool, position_ms: u32)
        -> oneshot::Receiver<()>
    {
        let (tx, rx) = oneshot::channel();
        self.command(PlayerCommand::Load(track, start_playing, position_ms, tx));

        rx
    }

    pub fn play(&self) {
        self.command(PlayerCommand::Play)
    }

    pub fn pause(&self) {
        self.command(PlayerCommand::Pause)
    }

    pub fn stop(&self) {
        self.command(PlayerCommand::Stop)
    }

    pub fn seek(&self, position_ms: u32) {
        self.command(PlayerCommand::Seek(position_ms));
    }

    pub fn fetch_audio_file(&self, track_id: SpotifyId, format: FileFormat) -> oneshot::Receiver<Result<Vec<u8>, Error>> {
        //TODO: I would actually like receiving this as a stream of chunks (64-512kB per chunk)
        fn do_fetch(session: &Session, track_id: SpotifyId, format: FileFormat) -> Result<Vec<u8>, Error> {
            let mut audio_file = match get_track_file(&session, track_id, format) {
                Some(data) => data,
                None => {
                    return Err(Error::new(ErrorKind::Other, format!("Cannot find file for track {:?} encoded as {:?}", track_id, format)));
                }
            };
            let file_len: u64 = audio_file.seek(SeekFrom::End(0)).unwrap();
            audio_file.seek(SeekFrom::Start(0)).unwrap();

            let mut full_buffer = Vec::with_capacity(file_len as usize);
            let mut buf = [0; 256*750];
            let mut tot: u64 = 0;
            loop {
                let n = audio_file.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                tot += n as u64;
                full_buffer.extend_from_slice(&buf[..n]);
            }
            if tot != file_len {
                return Err(Error::new(ErrorKind::Other, format!("File size mismatch: Expected {}, got {}", file_len, tot)));
            }
            Ok(full_buffer)
        }

        let (blob_tx, blob_rx) = oneshot::channel();
        let session = self.session.clone();
        thread::spawn(move || {
            let ret = do_fetch(&session, track_id, format);
            blob_tx.send(ret).unwrap();
        });

        blob_rx
    }

}

type DecryptedFile = AudioDecrypt<AudioFile>;
type Decoder = VorbisDecoder<Subfile<DecryptedFile>>;
enum PlayerState {
    Stopped,
    Paused {
        decoder: Decoder,
        end_of_track: oneshot::Sender<()>,
    },
    Playing {
        decoder: Decoder,
        end_of_track: oneshot::Sender<()>,
    },

    Invalid,
}

impl PlayerState {
    fn is_playing(&self) -> bool {
        use self::PlayerState::*;
        match *self {
            Stopped | Paused { .. } => false,
            Playing { .. } => true,
            Invalid => panic!("invalid state"),
        }
    }

    fn decoder(&mut self) -> Option<&mut Decoder> {
        use self::PlayerState::*;
        match *self {
            Stopped => None,
            Paused { ref mut decoder, .. } |
            Playing { ref mut decoder, .. } => Some(decoder),
            Invalid => panic!("invalid state"),
        }
    }

    fn signal_end_of_track(self) {
        use self::PlayerState::*;
        match self {
            Paused { end_of_track, .. } |
            Playing { end_of_track, .. } => {
                end_of_track.complete(())
            }

            Stopped => warn!("signal_end_of_track from stopped state"),
            Invalid => panic!("invalid state"),
        }
    }

    fn paused_to_playing(&mut self) {
        use self::PlayerState::*;
        match ::std::mem::replace(self, Invalid) {
            Paused { decoder, end_of_track } => {
                *self = Playing {
                    decoder: decoder,
                    end_of_track: end_of_track,
                };
            }
            _ => panic!("invalid state"),
        }
    }

    fn playing_to_paused(&mut self) {
        use self::PlayerState::*;
        match ::std::mem::replace(self, Invalid) {
            Playing { decoder, end_of_track } => {
                *self = Paused {
                    decoder: decoder,
                    end_of_track: end_of_track,
                };
            }
            _ => panic!("invalid state"),
        }
    }
}

impl PlayerInternal {
    fn run(mut self) {
        loop {
            let cmd = if self.state.is_playing() {
                match self.commands.try_recv() {
                    Ok(cmd) => Some(cmd),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                match self.commands.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(RecvError) => return,
                }
            };

            if let Some(cmd) = cmd {
                self.handle_command(cmd);
            }

            let packet = if let PlayerState::Playing { ref mut decoder, .. } = self.state {
                Some(decoder.next_packet().expect("Vorbis error"))
            } else { None };

            if let Some(packet) = packet {
                self.handle_packet(packet);
            }
        }
    }

    fn handle_packet(&mut self, packet: Option<VorbisPacket>) {
        match packet {
            Some(mut packet) => {
                if let Some(ref editor) = self.audio_filter {
                    editor.modify_stream(&mut packet.data_mut())
                };

                self.sink.write(&packet.data()).unwrap();
            }

            None => {
                self.sink.stop().unwrap();
                self.run_onstop();

                let old_state = mem::replace(&mut self.state, PlayerState::Stopped);
                old_state.signal_end_of_track();
            }
        }
    }

    fn handle_command(&mut self, cmd: PlayerCommand) {
        debug!("command={:?}", cmd);
        match cmd {
            PlayerCommand::Load(track_id, play, position, end_of_track) => {
                if self.state.is_playing() {
                    self.sink.stop().unwrap();
                }

                match self.load_track(track_id, position as i64) {
                    Some(decoder) => {
                        if play {
                            if !self.state.is_playing() {
                                self.run_onstart();
                            }
                            self.sink.start().unwrap();

                            self.state = PlayerState::Playing {
                                decoder: decoder,
                                end_of_track: end_of_track,
                            };
                        } else {
                            if self.state.is_playing() {
                                self.run_onstop();
                            }

                            self.state = PlayerState::Paused {
                                decoder: decoder,
                                end_of_track: end_of_track,
                            };
                        }
                    }

                    None => {
                        end_of_track.complete(());
                        if self.state.is_playing() {
                            self.run_onstop();
                        }
                    }
                }
            }

            PlayerCommand::Seek(position) => {
                if let Some(decoder) = self.state.decoder() {
                    match decoder.seek(position as i64) {
                        Ok(_) => (),
                        Err(err) => error!("Vorbis error: {:?}", err),
                    }
                } else {
                    warn!("Player::seek called from invalid state");
                }
            }

            PlayerCommand::Play => {
                if let PlayerState::Paused { .. } = self.state {
                    self.state.paused_to_playing();

                    self.run_onstart();
                    self.sink.start().unwrap();
                } else {
                    warn!("Player::play called from invalid state");
                }
            }

            PlayerCommand::Pause => {
                if let PlayerState::Playing { .. } = self.state {
                    self.state.playing_to_paused();

                    self.sink.stop().unwrap();
                    self.run_onstop();
                } else {
                    warn!("Player::pause called from invalid state");
                }
            }

            PlayerCommand::Stop => {
                match self.state {
                    PlayerState::Playing { .. } => {
                        self.sink.stop().unwrap();
                        self.run_onstop();
                        self.state = PlayerState::Stopped;
                    }
                    PlayerState::Paused { .. } => {
                        self.state = PlayerState::Stopped;
                    },
                    PlayerState::Stopped => {
                        warn!("Player::stop called from invalid state");
                    }
                    PlayerState::Invalid => panic!("invalid state"),
                }
            }
        }
    }

    fn run_onstart(&self) {
        if let Some(ref program) = self.config.onstart {
            util::run_program(program)
        }
    }

    fn run_onstop(&self) {
        if let Some(ref program) = self.config.onstop {
            util::run_program(program)
        }
    }

    fn load_track(&self, track_id: SpotifyId, position: i64) -> Option<Decoder> {
        let format = match self.config.bitrate {
            Bitrate::Bitrate96 => FileFormat::OGG_VORBIS_96,
            Bitrate::Bitrate160 => FileFormat::OGG_VORBIS_160,
            Bitrate::Bitrate320 => FileFormat::OGG_VORBIS_320,
        };

        let decrypted_file = match get_track_file(&self.session, track_id, format) {
            Some(data) => data,
            None => {
                return None;
            }
        };

        let audio_file = Subfile::new(decrypted_file, 0xa7);
        let mut decoder = VorbisDecoder::new(audio_file).unwrap();

        match decoder.seek(position) {
            Ok(_) => (),
            Err(err) => error!("Vorbis error: {:?}", err),
        }

        info!("Track \"{:?}\" loaded", track_id);

        Some(decoder)
    }
}

impl Drop for PlayerInternal {
    fn drop(&mut self) {
        debug!("drop Player[{}]", self.session.session_id());
    }
}

impl ::std::fmt::Debug for PlayerCommand {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match *self {
            PlayerCommand::Load(track, play, position, _) => {
                f.debug_tuple("Load")
                 .field(&track)
                 .field(&play)
                 .field(&position)
                 .finish()
            }
            PlayerCommand::Play => {
                f.debug_tuple("Play").finish()
            }
            PlayerCommand::Pause => {
                f.debug_tuple("Pause").finish()
            }
            PlayerCommand::Stop => {
                f.debug_tuple("Stop").finish()
            }
            PlayerCommand::Seek(position) => {
                f.debug_tuple("Seek")
                 .field(&position)
                 .finish()
            }
        }
    }
}

fn get_track_file(session: &Session, track_id: SpotifyId, format: FileFormat) -> Option<DecryptedFile> {
    let track = Track::get(&session, track_id).wait().unwrap();
    let track = match find_available_track_alternative(&session, &track) {
        Some(track) => track,
        None => {
            println!("Track \"{}\" is not available", track.name);
            return None;
        }
    };

    let file_id = match track.files.get(&format) {
        Some(&file_id) => file_id,
        None => {
            println!("Track \"{}\" is not available in format {:?}", track.name, format);
            return None;
        }
    };
    let key = session.audio_key().request(track.id, file_id).wait().unwrap();
    let encrypted_file = AudioFile::open(&session, file_id).wait().unwrap();
    Some(AudioDecrypt::new(key, encrypted_file))
}


fn find_available_track_alternative<'a>(session: &Session, track: &'a Track) -> Option<Cow<'a, Track>> {
    if track.available {
        Some(Cow::Borrowed(track))
    } else {
        let alternatives = track.alternatives
            .iter()
            .map(|alt_id| {
                Track::get(&session, *alt_id)
            });
        let alternatives = future::join_all(alternatives).wait().unwrap();

        alternatives.into_iter().find(|alt| alt.available).map(Cow::Owned)
    }
}
