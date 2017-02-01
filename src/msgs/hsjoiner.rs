
use std::collections::VecDeque;

use msgs::codec;
use msgs::tls_message::{TLSMessage, TLSMessagePayload};
use msgs::enums::{ContentType, ProtocolVersion};
use msgs::handshake::HandshakeMessagePayload;

const HEADER_SIZE: usize = 1 + 3;

/// This works to reconstruct TLS handshake messages
/// from individual TLS messages.  It's guaranteed that
/// TLS messages output from this layer contain precisely
/// one handshake payload.
pub struct HandshakeJoiner {
    /// Completed handshake frames for output.
    pub frames: VecDeque<TLSMessage>,

    /// The message payload we're currently accumulating.
    buf: Vec<u8>,
}

impl HandshakeJoiner {
    pub fn new() -> HandshakeJoiner {
        HandshakeJoiner {
            frames: VecDeque::new(),
            buf: Vec::new(),
        }
    }

    /// Do we want to process this message?
    pub fn want_message(&self, msg: &TLSMessage) -> bool {
        msg.is_content_type(ContentType::Handshake)
    }

    /// Do we have any buffered data?
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Take the message, and join/split it as needed.
    /// Return the number of new messages added to the
    /// output deque as a result of this message.
    ///
    /// Returns None if msg or a preceding message was corrupt.
    /// You cannot recover from this situation.  Otherwise returns
    /// a count of how many messages we queued.
    pub fn take_message(&mut self, mut msg: TLSMessage) -> Option<usize> {
        // Input must be opaque, otherwise we might have already
        // lost information!
        let payload = msg.take_opaque_payload().unwrap();

        self.buf.extend_from_slice(&payload.0[..]);

        let mut count = 0;
        while self.buf_contains_message() {
            if !self.deframe_one(msg.version) {
                return None;
            }

            count += 1;
        }

        Some(count)
    }

    /// Does our `buf` contain a full handshake payload?  It does if it is big
    /// enough to contain a header, and that header has a length which falls
    /// within `buf`.
    fn buf_contains_message(&self) -> bool {
        self.buf.len() >= HEADER_SIZE &&
        self.buf.len() >= (codec::decode_u24(&self.buf[1..4]).unwrap() as usize) + HEADER_SIZE
    }

    /// Take a TLS handshake payload off the front of `buf`, and put it onto
    /// the back of our `frames` deque inside a normal `Message`.
    ///
    /// Returns false if the stream is desynchronised beyond repair.
    fn deframe_one(&mut self, version: ProtocolVersion) -> bool {
        let used = {
            let mut rd = codec::Reader::init(&self.buf);
            let payload = HandshakeMessagePayload::read_version(&mut rd, version);

            if payload.is_none() {
                return false;
            }

            let m = TLSMessage {
                typ: ContentType::Handshake,
                version: version,
                payload: TLSMessagePayload::Handshake(payload.unwrap()),
            };

            self.frames.push_back(m);
            rd.used()
        };
        self.buf = self.buf.split_off(used);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::HandshakeJoiner;
    use msgs::enums::{ProtocolVersion, ContentType, HandshakeType};
    use msgs::handshake::{HandshakeMessagePayload, HandshakePayload};
    use msgs::tls_message::{TLSMessage, TLSMessagePayload};
    use msgs::base::Payload;

    #[test]
    fn want() {
        let hj = HandshakeJoiner::new();
        assert_eq!(hj.is_empty(), true);

        let wanted = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"hello world".to_vec()),
        };

        let unwanted = TLSMessage {
            typ: ContentType::Alert,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"ponytown".to_vec()),
        };

        assert_eq!(hj.want_message(&wanted), true);
        assert_eq!(hj.want_message(&unwanted), false);
    }

    fn pop_eq(expect: &TLSMessage, hj: &mut HandshakeJoiner) {
        let got = hj.frames.pop_front().unwrap();
        assert_eq!(got.typ, expect.typ);
        assert_eq!(got.version, expect.version);

        let (mut left, mut right) = (Vec::new(), Vec::new());
        got.payload.encode(&mut left);
        expect.payload.encode(&mut right);

        assert_eq!(left, right);
    }

    #[test]
    fn split() {
        // Check we split two handshake messages within one PDU.
        let mut hj = HandshakeJoiner::new();

        // two HelloRequests
        let msg = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"\x00\x00\x00\x00\x00\x00\x00\x00".to_vec()),
        };

        assert_eq!(hj.want_message(&msg), true);
        assert_eq!(hj.take_message(msg), Some(2));
        assert_eq!(hj.is_empty(), true);

        let expect = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::Handshake(HandshakeMessagePayload {
                typ: HandshakeType::HelloRequest,
                payload: HandshakePayload::HelloRequest,
            }),
        };

        pop_eq(&expect, &mut hj);
        pop_eq(&expect, &mut hj);
    }

    #[test]
    fn broken() {
        // Check obvious crap payloads are reported as errors, not panics.
        let mut hj = HandshakeJoiner::new();

        // short ClientHello
        let msg = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"\x01\x00\x00\x02\xff\xff".to_vec()),
        };

        assert_eq!(hj.want_message(&msg), true);
        assert_eq!(hj.take_message(msg), None);
    }

    #[test]
    fn join() {
        // Check we join one handshake message split over two PDUs.
        let mut hj = HandshakeJoiner::new();
        assert_eq!(hj.is_empty(), true);

        // Introduce Finished of 16 bytes, providing 4.
        let mut msg = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"\x14\x00\x00\x10\x00\x01\x02\x03\x04".to_vec()),
        };

        assert_eq!(hj.want_message(&msg), true);
        assert_eq!(hj.take_message(msg), Some(0));
        assert_eq!(hj.is_empty(), false);

        // 11 more bytes.
        msg = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e".to_vec()),
        };

        assert_eq!(hj.want_message(&msg), true);
        assert_eq!(hj.take_message(msg), Some(0));
        assert_eq!(hj.is_empty(), false);

        // Final 1 byte.
        msg = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::new_opaque(b"\x0f".to_vec()),
        };

        assert_eq!(hj.want_message(&msg), true);
        assert_eq!(hj.take_message(msg), Some(1));
        assert_eq!(hj.is_empty(), true);

        let payload = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f".to_vec();
        let expect = TLSMessage {
            typ: ContentType::Handshake,
            version: ProtocolVersion::TLSv1_2,
            payload: TLSMessagePayload::Handshake(HandshakeMessagePayload {
                typ: HandshakeType::Finished,
                payload: HandshakePayload::Finished(Payload::new(payload)),
            }),
        };

        pop_eq(&expect, &mut hj);
    }
}
