use std::fmt::{Display, Formatter};
use tokio::io::{AsyncWrite, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::Decoder;
use bytes::BytesMut;

const HDR_LEN: usize = 3;

#[derive(Clone, Copy)]
pub enum SubStreamLabel {
    Supervisor = 1,
    AppStdout = 2,
    AppStderr = 3,
}

impl SubStreamLabel {
    fn from_u8(val: u8) -> anyhow::Result<SubStreamLabel> {
        match val {
            1 => Ok(SubStreamLabel::Supervisor),
            2 => Ok(SubStreamLabel::AppStdout),
            3 => Ok(SubStreamLabel::AppStderr),
            _ => Err(anyhow::anyhow!("Unknown SubStreamLabel value: {val}"))
        }
    }
}

impl Display for SubStreamLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            SubStreamLabel::Supervisor => write!(f, "odyn"),
            SubStreamLabel::AppStdout => write!(f, "stdout"),
            SubStreamLabel::AppStderr => write!(f, "stderr"),
        }
    }
}

// Framing/serialization protocol is as follows. Each frame is composed of a 1 byte label
// followed by a 2 byte length (little endian), followed by the data bytes.

pub async fn write<W>(writer: &mut W, label: SubStreamLabel, data: &[u8]) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin
{
    // VsockStream does not support vectored write. To minimize the send calls
    // collect the label and data len into a single buffer.
    // data will be written out as a separate call as it can be large.

    let mut hdr: [u8; 1 + 2] = [label as u8, 0, 0];

    hdr[1..3].copy_from_slice(&((data.len() as u16).to_le_bytes()));

    writer.write_all(&hdr).await?;
    writer.write_all(data).await?;

    Ok(())
}

pub async fn read<R>(reader: &mut R) -> anyhow::Result<(SubStreamLabel, Vec<u8>)>
where
    R: AsyncRead + Unpin
{
    let mut hdr: [u8; 3] = [0u8; 3];
    reader.read_exact(&mut hdr).await?;

    let label = SubStreamLabel::from_u8(hdr[0])?;
    let data_len = u16::from_le_bytes(hdr[1..3].try_into().unwrap());

    let mut data = vec![0u8; data_len as usize];
    reader.read_exact(&mut data).await?;

    Ok((label, data))
}

pub struct SubStreamCodec;

impl SubStreamCodec {
    pub fn new() -> Self {
        Self
    }
}

impl Decoder for SubStreamCodec {
    type Item = (SubStreamLabel, BytesMut);
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // check that at least header is there
        if src.len() < HDR_LEN {
            return Ok(None)
        }

        let label = SubStreamLabel::from_u8(src[0])?;
        let data_len = u16::from_le_bytes(src[1..3].try_into().unwrap());
        let frame_len = HDR_LEN + data_len as usize;

        if src.len() < frame_len {
            return Ok(None)
        }

        // both the header and data are there, can return
        let data = src.split_to(frame_len);
        Ok(Some((label, data)))
    }
}
