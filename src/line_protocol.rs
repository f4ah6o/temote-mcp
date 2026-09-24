use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub(crate) const MAX_JSON_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BoundedLine {
    Line(String),
    TooLarge,
    InvalidUtf8,
}

pub(crate) async fn next_bounded_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut saw_any = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !saw_any {
                return Ok(None);
            }
            break;
        }
        saw_any = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if !oversized {
            if bytes.len().saturating_add(consumed) > max_bytes {
                oversized = true;
            } else {
                bytes.extend_from_slice(&available[..consumed]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }

    if oversized {
        return Ok(Some(BoundedLine::TooLarge));
    }
    match String::from_utf8(bytes) {
        Ok(line) => Ok(Some(BoundedLine::Line(line))),
        Err(_) => Ok(Some(BoundedLine::InvalidUtf8)),
    }
}
