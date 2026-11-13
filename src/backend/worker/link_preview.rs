//! WhatsApp-hosted pictures of link previews: their download parameters and
//! the recovery of older previews' metadata from retained protobufs.

use whatsapp_rust::download::{DownloadParams, MediaType};
use whatsapp_rust::prelude::{MessageExt, wa};

impl super::Worker {
    /// Recover preview metadata from retained protobufs without rederiving
    /// other message kinds or overwriting edited text and downloaded files.
    pub(super) fn backfill_link_previews(&mut self) {
        const KEY: &str = "link_preview_images_v1";
        if self.archive.meta(KEY).ok().flatten().as_deref() == Some("1") {
            return;
        }
        let mut complete = true;
        let mut after = 0;
        loop {
            let Ok(rows) = self.archive.link_previews_with_raw(after) else {
                return;
            };
            if rows.is_empty() {
                break;
            }
            for crate::archive::RawMessage {
                rowid,
                chat,
                id,
                raw,
            } in rows
            {
                after = rowid;
                let Some(raw) = raw else {
                    continue;
                };
                use whatsapp_rust::waproto::buffa::Message as _;
                let Ok(raw) = wa::Message::decode_from_slice(&raw) else {
                    continue;
                };
                if !raw.get_base_message().extended_text_message.is_set() {
                    continue;
                }
                let Ok(Some(mut existing)) = self.archive.message(&chat, &id) else {
                    complete = false;
                    continue;
                };
                if existing.edited
                    || !matches!(existing.content, crate::model::Content::Text { .. })
                {
                    continue;
                }
                let Some(mut content) = super::classify(&raw) else {
                    continue;
                };
                if !matches!(
                    content,
                    crate::model::Content::Text {
                        preview: Some(_),
                        ..
                    }
                ) {
                    continue;
                }
                content.keep_local_paths(&existing.content);
                if let (
                    crate::model::Content::Text { preview: new, .. },
                    crate::model::Content::Text { preview, .. },
                ) = (&mut content, &mut existing.content)
                {
                    std::mem::swap(preview, new);
                }
                let thumbnail = existing
                    .thumbnail
                    .or_else(|| super::thumbnail_of(raw.get_base_message()));
                if self
                    .archive
                    .set_derived(
                        &chat,
                        &id,
                        &existing.content,
                        &existing.mentions,
                        thumbnail.as_deref(),
                        existing.forwarded,
                    )
                    .is_err()
                {
                    complete = false;
                }
            }
        }
        if complete {
            let _ = self.archive.set_meta(KEY, "1");
        }
    }
}

/// Use the protocol library to download and verify WhatsApp's encrypted link
/// picture, including messages with no small inline JPEG at all.
pub(super) fn picture(text: &wa::message::ExtendedTextMessage) -> Option<DownloadParams> {
    let path = text
        .thumbnail_direct_path
        .as_deref()
        .filter(|path| !path.is_empty())?;
    DownloadParams::encrypted(
        path,
        text.media_key.as_deref()?,
        text.thumbnail_sha256.as_deref()?,
        text.thumbnail_enc_sha256.as_deref()?,
        0,
        MediaType::LinkThumbnail,
    )
    .ok()
}

/// Bound decoded pixels before egui opens a downloaded preview. Reading the
/// header keeps full-quality pixels without decoding the image twice.
pub(super) fn valid_picture(path: &std::path::Path) -> bool {
    image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok())
        .and_then(|reader| reader.into_dimensions().ok())
        .is_some_and(|(width, height)| {
            width > 0
                && height > 0
                && width <= MAX_SOURCE_SIDE
                && height <= MAX_SOURCE_SIDE
                // Allow for 16-bit RGBA sources as well as 8-bit JPEGs.
                && u64::from(width) * u64::from(height) * 8 <= MAX_DECODE_BYTES
        })
}

/// The widest or tallest picture worth decoding, and the most memory its
/// decoder may ask for.
const MAX_SOURCE_SIDE: u32 = 8192;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;

/// The largest link picture the worker fetches itself.
pub(super) const PICTURE_LIMIT: u64 = 2 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::super::receipt_tests::{PEER, own_message, worker};
    use whatsapp_rust::prelude::wa;
    use whatsapp_rust::waproto::buffa::Message as _;

    /// A text with its preview stripped, filed with the raw that names it:
    /// backfill restores the title without touching the read state.
    #[test]
    fn backfill_recovers_preview_metadata_without_touching_read_state() {
        let (mut worker, _events, _commands, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Demo").unwrap();
        let raw = wa::Message {
            extended_text_message: whatsapp_rust::prelude::MessageField::some(
                wa::message::ExtendedTextMessage {
                    text: Some("see https://example.com/a".into()),
                    matched_text: Some("https://example.com/a".into()),
                    title: Some("Example".into()),
                    ..Default::default()
                },
            ),
            ..Default::default()
        };
        let raw = raw.encode_to_vec();
        let mut placeholder = own_message("link", 123);
        placeholder.content = crate::model::Content::text("see https://example.com/a");
        placeholder.read_at = Some(456);
        worker
            .archive
            .insert_message(&placeholder, Some(&raw))
            .unwrap();
        worker.backfill_link_previews();
        let recovered = worker.archive.message(PEER, "link").unwrap().unwrap();
        match &recovered.content {
            crate::model::Content::Text {
                preview: Some(preview),
                ..
            } => {
                assert_eq!(preview.url, "https://example.com/a");
                assert_eq!(preview.title.as_deref(), Some("Example"));
            }
            other => panic!("no preview recovered: {other:?}"),
        }
        assert_eq!(recovered.read_at, Some(456));
        // A second run finds nothing to do.
        worker.backfill_link_previews();
        assert_eq!(
            worker
                .archive
                .message(PEER, "link")
                .unwrap()
                .unwrap()
                .content,
            recovered.content
        );
    }
}
