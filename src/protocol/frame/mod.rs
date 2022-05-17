//! Utilities to work with raw WebSocket frames.

pub mod coding;

#[allow(clippy::module_inception)]
mod frame;
mod mask;

use std::io::{Error as IoError, ErrorKind as IoErrorKind, IoSlice, Read, Write};

use bytes::{Buf, BufMut, BytesMut};
use log::*;

pub use self::frame::{CloseFrame, Frame, FrameHeader, PreparedFrame};
use crate::{
    buffer::BytesList,
    error::{CapacityError, Error, Result},
    ReadBuffer,
};

/// A reader and writer for WebSocket frames.
#[derive(Debug)]
pub struct FrameSocket<Stream> {
    /// The underlying network stream.
    stream: Stream,
    /// Codec for reading/writing frames.
    codec: FrameCodec,
}

impl<Stream> FrameSocket<Stream> {
    /// Create a new frame socket.
    pub fn new(stream: Stream) -> Self {
        FrameSocket { stream, codec: FrameCodec::new() }
    }

    /// Create a new frame socket from partially read data.
    pub fn from_partially_read(stream: Stream, part: Vec<u8>) -> Self {
        FrameSocket { stream, codec: FrameCodec::from_partially_read(part) }
    }

    /// Extract a stream from the socket.
    pub fn into_inner(self) -> (Stream, Vec<u8>) {
        (self.stream, self.codec.in_buffer.into_vec())
    }

    /// Returns a shared reference to the inner stream.
    pub fn get_ref(&self) -> &Stream {
        &self.stream
    }

    /// Returns a mutable reference to the inner stream.
    pub fn get_mut(&mut self) -> &mut Stream {
        &mut self.stream
    }
}

impl<Stream> FrameSocket<Stream>
where
    Stream: Read,
{
    /// Read a frame from stream.
    pub fn read_frame(&mut self, max_size: Option<usize>) -> Result<Option<Frame>> {
        self.codec.read_frame(&mut self.stream, max_size)
    }
}

impl<Stream> FrameSocket<Stream>
where
    Stream: Write,
{
    /// Write a frame to the stream.
    ///
    /// This function guarantees that the frame is queued regardless of any errors.
    /// There is no need to resend the frame. In order to handle WouldBlock or Incomplete,
    /// call write_pending() afterwards.
    pub fn write_frame(&mut self, frame: Frame) -> Result<()> {
        self.codec.write_frame(&mut self.stream, frame)
    }

    /// Write a prepared frame to the stream.
    ///
    /// Only servers can send prepared frames, because clients must apply a unique mask to each
    /// frame they send, so each frame is different on the wire.
    ///
    /// This function guarantees that the frame is queued regardless of any errors.
    /// There is no need to resend the frame. In order to handle WouldBlock or Incomplete,
    /// call write_pending() afterwards.
    pub fn write_prepared_frame(&mut self, frame: &PreparedFrame) -> Result<()> {
        self.codec.write_prepared_frame(&mut self.stream, frame)
    }

    /// Complete pending write, if any.
    pub fn write_pending(&mut self) -> Result<()> {
        self.codec.write_pending(&mut self.stream)
    }
}

/// A codec for WebSocket frames.
#[derive(Debug)]
pub(super) struct FrameCodec {
    /// Buffer to read data from the stream.
    in_buffer: ReadBuffer,
    /// Queue of buffers to write to the stream.
    /// Can contain a mix of slices from scratch_buffer and PreparedFrames.
    out_buffer: BytesList,
    /// Buffer used to prepare frames to write to the stream.
    /// As slices are released from out_buffer, BytesMut should be able to reuse allocations.
    scratch_buffer: BytesMut,
    /// Header and remaining size of the incoming packet being processed.
    header: Option<(FrameHeader, u64)>,
}

impl FrameCodec {
    /// Create a new frame codec.
    pub(super) fn new() -> Self {
        Self {
            in_buffer: ReadBuffer::new(),
            out_buffer: BytesList::default(),
            scratch_buffer: BytesMut::new(),
            header: None,
        }
    }

    /// Create a new frame codec from partially read data.
    pub(super) fn from_partially_read(part: Vec<u8>) -> Self {
        Self {
            in_buffer: ReadBuffer::from_partially_read(part),
            out_buffer: BytesList::default(),
            scratch_buffer: BytesMut::new(),
            header: None,
        }
    }

    /// Read a frame from the provided stream.
    pub(super) fn read_frame<Stream>(
        &mut self,
        stream: &mut Stream,
        max_size: Option<usize>,
    ) -> Result<Option<Frame>>
    where
        Stream: Read,
    {
        let max_size = max_size.unwrap_or_else(usize::max_value);

        let payload = loop {
            {
                let cursor = self.in_buffer.as_cursor_mut();

                if self.header.is_none() {
                    self.header = FrameHeader::parse(cursor)?;
                }

                if let Some((_, ref length)) = self.header {
                    let length = *length;

                    // Enforce frame size limit early and make sure `length`
                    // is not too big (fits into `usize`).
                    if length > max_size as u64 {
                        return Err(Error::Capacity(CapacityError::MessageTooLong {
                            size: length as usize,
                            max_size,
                        }));
                    }

                    let input_size = cursor.get_ref().len() as u64 - cursor.position();
                    if length <= input_size {
                        // No truncation here since `length` is checked above
                        let mut payload = Vec::with_capacity(length as usize);
                        if length > 0 {
                            Read::take(cursor, length).read_to_end(&mut payload)?;
                        }
                        break payload;
                    }
                }
            }

            // Not enough data in buffer.
            let size = self.in_buffer.read_from(stream)?;
            if size == 0 {
                trace!("no frame received");
                return Ok(None);
            }
        };

        let (header, length) = self.header.take().expect("Bug: no frame header");
        debug_assert_eq!(payload.len() as u64, length);
        let frame = Frame::from_payload(header, payload);
        trace!("received frame {}", frame);
        Ok(Some(frame))
    }

    /// Write a frame to the provided stream.
    pub(super) fn write_frame<Stream>(&mut self, stream: &mut Stream, frame: Frame) -> Result<()>
    where
        Stream: Write,
    {
        trace!("writing frame {}", frame);
        self.scratch_buffer.reserve(frame.len());
        let mut writer = (&mut self.scratch_buffer).writer();
        frame.format(&mut writer).expect("Bug: can't write to buffer");
        self.out_buffer.push(self.scratch_buffer.split().freeze());
        self.write_pending(stream)
    }

    /// Write a prepared frame to the provided stream.
    pub(super) fn write_prepared_frame<Stream>(
        &mut self,
        stream: &mut Stream,
        frame: &PreparedFrame,
    ) -> Result<()>
    where
        Stream: Write,
    {
        trace!("writing prepared frame {}", frame);
        self.out_buffer.push(frame.header().clone());
        self.out_buffer.push(frame.payload().clone());
        self.write_pending(stream)
    }

    /// Complete pending write, if any.
    pub(super) fn write_pending<Stream>(&mut self, stream: &mut Stream) -> Result<()>
    where
        Stream: Write,
    {
        loop {
            let mut ioslices = [IoSlice::new(&[]); 32];
            let num_ioslices = self.out_buffer.chunks_vectored(&mut ioslices);
            if num_ioslices == 0 {
                break;
            }

            let len = stream.write_vectored(&ioslices[..num_ioslices])?;
            if len == 0 {
                // This is the same as "Connection reset by peer"
                return Err(IoError::new(
                    IoErrorKind::ConnectionReset,
                    "Connection reset while sending",
                )
                .into());
            }
            self.out_buffer.advance(len);
        }
        stream.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use crate::error::{CapacityError, Error};

    use super::{Frame, FrameSocket};

    use std::io::Cursor;

    #[test]
    fn read_frames() {
        let raw = Cursor::new(vec![
            0x82, 0x07, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x82, 0x03, 0x03, 0x02, 0x01,
            0x99,
        ]);
        let mut sock = FrameSocket::new(raw);

        assert_eq!(
            sock.read_frame(None).unwrap().unwrap().into_data(),
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]
        );
        assert_eq!(sock.read_frame(None).unwrap().unwrap().into_data(), vec![0x03, 0x02, 0x01]);
        assert!(sock.read_frame(None).unwrap().is_none());

        let (_, rest) = sock.into_inner();
        assert_eq!(rest, vec![0x99]);
    }

    #[test]
    fn from_partially_read() {
        let raw = Cursor::new(vec![0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        let mut sock = FrameSocket::from_partially_read(raw, vec![0x82, 0x07, 0x01]);
        assert_eq!(
            sock.read_frame(None).unwrap().unwrap().into_data(),
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]
        );
    }

    #[test]
    fn write_frames() {
        let mut sock = FrameSocket::new(Vec::new());

        let frame = Frame::ping(vec![0x04, 0x05]);
        sock.write_frame(frame).unwrap();

        let frame = Frame::pong(vec![0x01]);
        sock.write_frame(frame).unwrap();

        let (buf, _) = sock.into_inner();
        assert_eq!(buf, vec![0x89, 0x02, 0x04, 0x05, 0x8a, 0x01, 0x01]);
    }

    #[test]
    fn parse_overflow() {
        let raw = Cursor::new(vec![
            0x83, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00,
        ]);
        let mut sock = FrameSocket::new(raw);
        let _ = sock.read_frame(None); // should not crash
    }

    #[test]
    fn size_limit_hit() {
        let raw = Cursor::new(vec![0x82, 0x07, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        let mut sock = FrameSocket::new(raw);
        assert!(matches!(
            sock.read_frame(Some(5)),
            Err(Error::Capacity(CapacityError::MessageTooLong { size: 7, max_size: 5 }))
        ));
    }
}
