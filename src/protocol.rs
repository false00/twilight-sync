use crate::{config::PlayLock, files::SAVE_SIZE};
use anyhow::{Context, Result, ensure};
use serde::{
    Deserialize, Serialize,
    de::{self, SeqAccess, Visitor},
};
use snow::{HandshakeState, TransportState, params::NoiseParams};
use std::{
    fmt,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const PORT: u16 = 42817;
pub const DISCOVERY_PORT: u16 = 42816;
pub const MAX_FRAME: usize = 65_535;
const REKEY_INTERVAL: u64 = 4096;
const PREFACE: &[u8; 4] = b"TWP1";

#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    Hello {
        name: String,
    },
    ApprovePairing,
    PairingReady,
    State {
        hash: Option<String>,
        save_ready: bool,
        game: bool,
        lock: Option<PlayLock>,
    },
    Save {
        hash: String,
        base: Option<String>,
        expected: Option<String>,
        manual: bool,
        lease: Option<String>,
        #[serde(deserialize_with = "bounded_bytes")]
        bytes: Vec<u8>,
    },
    Saved {
        hash: String,
    },
    LeaseRequest {
        token: String,
        hash: Option<String>,
    },
    LeaseGranted {
        token: String,
    },
    LeaseDenied {
        token: String,
        reason: String,
    },
    LeaseRelease {
        token: String,
        hash: Option<String>,
    },
    LeaseReleased {
        token: String,
    },
    Recover {
        token: String,
    },
    RecoveryApproved {
        token: String,
    },
    IsoOffer {
        id: String,
        hash: String,
        size: u64,
    },
    IsoAccept {
        id: String,
    },
    IsoChunk {
        id: String,
        #[serde(deserialize_with = "bounded_bytes")]
        bytes: Vec<u8>,
    },
    IsoDone {
        id: String,
    },
    IsoReceived {
        id: String,
    },
    Error {
        message: String,
    },
    Ping,
    // New variants stay at the end so existing twilightProto/1 event indexes do not change.
    RvzOffer {
        id: String,
        hash: String,
        size: u64,
    },
}

fn bounded_bytes<'de, D: de::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<u8>, D::Error> {
    struct Bounded;
    impl<'de> Visitor<'de> for Bounded {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a bounded binary payload")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            if seq.size_hint().is_some_and(|size| size > SAVE_SIZE) {
                return Err(de::Error::custom("binary payload exceeds limit"));
            }
            let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(SAVE_SIZE));
            while let Some(byte) = seq.next_element()? {
                if bytes.len() == SAVE_SIZE {
                    return Err(de::Error::custom("binary payload exceeds limit"));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
    deserializer.deserialize_seq(Bounded)
}

pub fn noise_params() -> Result<NoiseParams> {
    "Noise_XX_25519_ChaChaPoly_SHA256"
        .parse()
        .context("Invalid Noise parameters")
}

pub struct SecureReader {
    stream: TcpStream,
    state: Arc<Mutex<TransportState>>,
    received: u64,
}

pub struct SecureWriter {
    stream: TcpStream,
    state: Arc<Mutex<TransportState>>,
    sent: u64,
}

pub struct Handshake {
    pub reader: SecureReader,
    pub writer: SecureWriter,
    pub remote_key: String,
    pub comparison_code: String,
}

fn read_packet(stream: &mut TcpStream, limit: usize) -> Result<Vec<u8>> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (16..=limit).contains(&length),
        "Invalid twilightProto frame length"
    );
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn write_packet(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_FRAME, "Frame exceeds protocol limit");
    stream.write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
    stream.write_all(bytes)?;
    Ok(())
}

fn noise_write(state: &mut HandshakeState, stream: &mut TcpStream) -> Result<()> {
    let mut buffer = [0; 1024];
    let length = state.write_message(&[], &mut buffer)?;
    write_packet(stream, &buffer[..length])
}

fn noise_read(state: &mut HandshakeState, stream: &mut TcpStream) -> Result<()> {
    let bytes = read_packet(stream, 1024)?;
    let mut payload = [0; 1024];
    ensure!(
        state.read_message(&bytes, &mut payload)? == 0,
        "Unexpected handshake payload"
    );
    Ok(())
}

pub fn handshake(
    mut stream: TcpStream,
    private: &[u8],
    network: &str,
    initiator: bool,
) -> Result<Handshake> {
    ensure!(crate::files::valid_hash(network), "Invalid network binding");
    stream
        .set_nonblocking(false)
        .context("Switching twilightProto channel to blocking I/O")?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let prologue = format!("twilightProto/1|{network}");
    let builder = snow::Builder::new(noise_params()?)
        .local_private_key(private)?
        .prologue(prologue.as_bytes())?;
    let mut state = if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    };
    if initiator {
        stream.write_all(PREFACE)?;
        noise_write(&mut state, &mut stream)?;
        noise_read(&mut state, &mut stream)?;
        noise_write(&mut state, &mut stream)?;
    } else {
        let mut preface = [0; 4];
        stream.read_exact(&mut preface)?;
        ensure!(&preface == PREFACE, "Not twilightProto version 1");
        noise_read(&mut state, &mut stream)?;
        noise_write(&mut state, &mut stream)?;
        noise_read(&mut state, &mut stream)?;
    }
    ensure!(state.is_handshake_finished(), "Incomplete Noise handshake");
    let remote_key = hex::encode(state.get_remote_static().context("Missing peer identity")?);
    let code = u64::from_be_bytes(state.get_handshake_hash()[..8].try_into()?) % 100_000_000;
    let comparison_code = format!("{:04} {:04}", code / 10_000, code % 10_000);
    let state = Arc::new(Mutex::new(state.into_transport_mode()?));
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    let writer = SecureWriter {
        stream: stream.try_clone()?,
        state: state.clone(),
        sent: 0,
    };
    Ok(Handshake {
        reader: SecureReader {
            stream,
            state,
            received: 0,
        },
        writer,
        remote_key,
        comparison_code,
    })
}

impl SecureWriter {
    pub fn send(&mut self, event: &Event) -> Result<()> {
        let bytes = postcard::to_allocvec(event)?;
        ensure!(
            bytes.len() <= MAX_FRAME - 16,
            "Event exceeds protocol limit"
        );
        let mut encrypted = vec![0; bytes.len() + 16];
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Cipher lock poisoned"))?;
        let length = state.write_message(&bytes, &mut encrypted)?;
        self.sent += 1;
        if self.sent.is_multiple_of(REKEY_INTERVAL) {
            state.rekey_outgoing();
        }
        drop(state);
        write_packet(&mut self.stream, &encrypted[..length])
    }

    pub fn close(&self) -> Result<()> {
        self.stream
            .shutdown(Shutdown::Both)
            .context("Closing secure channel")
    }
}

impl SecureReader {
    pub fn receive(&mut self) -> Result<Event> {
        let bytes = read_packet(&mut self.stream, MAX_FRAME)?;
        let mut plaintext = vec![0; bytes.len()];
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Cipher lock poisoned"))?;
        let length = state.read_message(&bytes, &mut plaintext)?;
        self.received += 1;
        if self.received.is_multiple_of(REKEY_INTERVAL) {
            state.rekey_incoming();
        }
        drop(state);
        let (event, remaining) = postcard::take_from_bytes(&plaintext[..length])?;
        ensure!(remaining.is_empty(), "Trailing data in event");
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    #[test]
    fn bilateral_encrypted_session_and_rekey() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let network = "a".repeat(64);
        let peer_network = network.clone();
        let left = snow::Builder::new(noise_params()?).generate_keypair()?;
        let right = snow::Builder::new(noise_params()?).generate_keypair()?;
        let left_public = hex::encode(left.public);
        let right_public = hex::encode(right.public);
        let worker = thread::spawn(move || -> Result<String> {
            let (stream, _) = listener.accept()?;
            let mut session = handshake(stream, &right.private, &peer_network, false)?;
            ensure!(
                session.remote_key == left_public,
                "Wrong initiator identity"
            );
            for _ in 0..REKEY_INTERVAL + 1 {
                ensure!(
                    matches!(session.reader.receive()?, Event::Ping),
                    "Wrong event"
                );
            }
            session.writer.send(&Event::Saved {
                hash: "b".repeat(64),
            })?;
            Ok(session.comparison_code)
        });
        let mut session = handshake(TcpStream::connect(address)?, &left.private, &network, true)?;
        assert_eq!(session.remote_key, right_public);
        for _ in 0..REKEY_INTERVAL + 1 {
            session.writer.send(&Event::Ping)?;
        }
        assert!(matches!(session.reader.receive()?, Event::Saved { .. }));
        let other_code = worker
            .join()
            .map_err(|_| anyhow::anyhow!("Peer thread panicked"))??;
        assert_eq!(session.comparison_code, other_code);
        Ok(())
    }

    #[test]
    fn accepted_nonblocking_socket_waits_for_pairing_traffic() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (stream, _) = listener.accept()?;
        stream.set_nonblocking(true)?;
        let left = snow::Builder::new(noise_params()?).generate_keypair()?;
        let right = snow::Builder::new(noise_params()?).generate_keypair()?;
        let network = "a".repeat(64);
        let peer_network = network.clone();
        let worker = thread::spawn(move || -> Result<String> {
            let mut session = handshake(stream, &right.private, &peer_network, false)?;
            ensure!(
                matches!(session.reader.receive()?, Event::Hello { name } if name == "Alice"),
                "Wrong pairing hello"
            );
            session.writer.send(&Event::Hello { name: "Bob".into() })?;
            Ok(session.comparison_code)
        });
        thread::sleep(Duration::from_millis(50));
        if worker.is_finished() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Peer thread panicked"))??;
            anyhow::bail!("Handshake completed before the peer sent its preface");
        }
        let mut session = handshake(client, &left.private, &network, true)?;
        thread::sleep(Duration::from_millis(50));
        session.writer.send(&Event::Hello {
            name: "Alice".into(),
        })?;
        ensure!(
            matches!(session.reader.receive()?, Event::Hello { name } if name == "Bob"),
            "Wrong pairing hello"
        );
        let other_code = worker
            .join()
            .map_err(|_| anyhow::anyhow!("Peer thread panicked"))??;
        assert_eq!(session.comparison_code, other_code);
        Ok(())
    }

    #[test]
    fn different_router_bindings_cannot_handshake() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let keys = snow::Builder::new(noise_params()?).generate_keypair()?;
        let worker = thread::spawn(move || -> Result<bool> {
            let (stream, _) = listener.accept()?;
            Ok(handshake(stream, &keys.private, &"a".repeat(64), false).is_err())
        });
        let keys = snow::Builder::new(noise_params()?).generate_keypair()?;
        assert!(
            handshake(
                TcpStream::connect(address)?,
                &keys.private,
                &"b".repeat(64),
                true
            )
            .is_err()
        );
        assert!(
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Peer thread panicked"))??
        );
        Ok(())
    }

    #[test]
    fn rejects_ciphertext_tampering_and_replays() -> Result<()> {
        // NN isolates transport tamper/replay checks from identity provisioning.
        let nn: NoiseParams = "Noise_NN_25519_ChaChaPoly_SHA256".parse()?;
        let mut left = snow::Builder::new(nn.clone()).build_initiator()?;
        let mut right = snow::Builder::new(nn).build_responder()?;
        let mut buffer = [0; 1024];
        let mut out = [0; 1024];
        let count = left.write_message(&[], &mut buffer)?;
        right.read_message(&buffer[..count], &mut out)?;
        let count = right.write_message(&[], &mut buffer)?;
        left.read_message(&buffer[..count], &mut out)?;
        let (mut left, mut right) = (left.into_transport_mode()?, right.into_transport_mode()?);
        let count = left.write_message(b"save", &mut buffer)?;
        let mut tampered = buffer[..count].to_vec();
        tampered[0] ^= 1;
        assert!(right.read_message(&tampered, &mut out).is_err());
        right.read_message(&buffer[..count], &mut out)?;
        assert!(right.read_message(&buffer[..count], &mut out).is_err());
        Ok(())
    }

    #[test]
    fn rvz_appends_an_event_without_changing_legacy_disc_frames() -> Result<()> {
        let id = "a".repeat(64);
        let hash = "b".repeat(64);
        for (index, event) in [
            (
                13,
                Event::IsoOffer {
                    id: id.clone(),
                    hash: hash.clone(),
                    size: crate::files::ISO_SIZE,
                },
            ),
            (14, Event::IsoAccept { id: id.clone() }),
            (
                15,
                Event::IsoChunk {
                    id: id.clone(),
                    bytes: vec![7; 32 * 1024],
                },
            ),
            (16, Event::IsoDone { id: id.clone() }),
            (17, Event::IsoReceived { id: id.clone() }),
            (
                18,
                Event::Error {
                    message: "declined".into(),
                },
            ),
            (19, Event::Ping),
            (
                20,
                Event::RvzOffer {
                    id: id.clone(),
                    hash: hash.clone(),
                    size: 1024,
                },
            ),
        ] {
            let bytes = postcard::to_allocvec(&event)?;
            assert_eq!(bytes[0], index);
            let restored: Event = postcard::from_bytes(&bytes)?;
            assert_eq!(
                std::mem::discriminant(&event),
                std::mem::discriminant(&restored)
            );
            if index == 20 {
                assert!(
                    matches!(restored, Event::RvzOffer { id: actual_id, hash: actual_hash, size: 1024 } if actual_id == id && actual_hash == hash)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn oversized_binary_declarations_are_rejected_before_allocation() -> Result<()> {
        let bytes = postcard::to_allocvec(&Event::IsoChunk {
            id: "a".repeat(64),
            bytes: vec![0; SAVE_SIZE + 1],
        })?;
        assert!(postcard::from_bytes::<Event>(&bytes).is_err());
        Ok(())
    }

    #[test]
    fn frame_length_is_bounded_before_reading_a_body() -> Result<()> {
        for length in [0_u32, 15, MAX_FRAME as u32 + 1, u32::MAX] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let mut sender = TcpStream::connect(listener.local_addr()?)?;
            sender.write_all(&length.to_be_bytes())?;
            let (mut receiver, _) = listener.accept()?;
            assert!(read_packet(&mut receiver, MAX_FRAME).is_err());
        }
        Ok(())
    }
}
