//! Colour emoji in the platform's own style, from fastframe-emoji: Apple
//! Color Emoji on macOS, Segoe UI Emoji on Windows, the desktop's emoji font
//! on Linux, and the bundled Noto Color Emoji for whatever they lack (the
//! country flags Segoe UI Emoji has none of, or a desktop without an emoji
//! font).
//!
//! Layout replaces each emoji sequence with a transparent placeholder, and
//! its picture is painted over it. New pictures are drawn on a worker
//! thread, so a picker full of unseen emoji never stalls a frame; tests and
//! demo builds draw them inside the frame so every screenshot shows them.
//! These wrappers install Zap Fasterk's choice before the first use.

use egui::text::LayoutJob;
use egui::{Pos2, Rect, TextFormat};

pub use fastframe_emoji::{PLACEHOLDER, only_emoji};

/// Noto Color Emoji, behind the platform's font.
const BUNDLED: &[u8] = include_bytes!("../assets/fonts/NotoColorEmoji.ttf");

fn setup_for(renderer: crate::settings::EmojiRenderer) -> fastframe_emoji::EmojiSetup {
    fastframe_emoji::EmojiSetup::default()
        .system(renderer == crate::settings::EmojiRenderer::System)
        .bundled(BUNDLED)
        .synchronous(cfg!(any(test, feature = "demo")))
}

/// Installs Zap Fasterk's configured emoji source before any renderer can initialize.
pub fn setup(renderer: crate::settings::EmojiRenderer) {
    setup_with_font(renderer, None);
}

static STARTUP_ERROR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Why a configured custom font was replaced by the selected built-in renderer.
pub fn startup_error() -> Option<&'static str> {
    STARTUP_ERROR.get().and_then(|error| error.as_deref())
}

/// Installs one validated user-supplied bitmap font for this process's lifetime.
/// The public renderer API accepts static bytes and cannot change after startup.
pub fn setup_with_font(renderer: crate::settings::EmojiRenderer, custom: Option<&std::path::Path>) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let (setup, error) = match custom.map(crate::custom_emoji::load) {
            Some(Ok(bytes)) => {
                // Once for the lifetime of the process, matching fastframe's
                // static renderer. Rejected imports never retain allocations.
                let bytes = Box::leak(bytes.into_boxed_slice());
                (
                    fastframe_emoji::EmojiSetup::default()
                        .system(false)
                        .bundled(bytes)
                        .synchronous(cfg!(any(test, feature = "demo"))),
                    None,
                )
            }
            Some(Err(error)) => (setup_for(renderer), Some(error)),
            None => (setup_for(renderer), None),
        };
        let _ = STARTUP_ERROR.set(error);
        setup.install();
    });
}

/// Whether a colour emoji font is available.
pub fn available() -> bool {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::available()
}

/// Finds and maps the emoji fonts before the first frame needs them.
pub fn warm_up() {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::warm_up();
}

/// The egui plugin that colours the emoji in every other text: labels,
/// buttons, menus, tooltips and text fields. It leaves the placeholders
/// `append` lays out and the glyphs `editor_job` hides to the paint calls
/// below, so message bodies keep their selectable, copyable placeholders.
pub fn plugin() -> fastframe_emoji::EmojiPlugin {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::EmojiPlugin::default()
}

/// Queues pictures to be drawn off the interface thread before a frame shows
/// them: the first page of a picker as it opens.
pub fn prewarm<'a>(ctx: &egui::Context, clusters: impl IntoIterator<Item = &'a str>) {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::prewarm(ctx, clusters);
}

/// Appends text with placeholders and records their emoji sequences.
pub fn append(
    ui: &egui::Ui,
    job: &mut LayoutJob,
    placements: &mut Vec<String>,
    text: &str,
    format: &TextFormat,
) -> usize {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::append(ui, job, placements, text, format)
}

/// Paints one emoji over `rect`, or its text when no font draws it.
pub fn paint_cluster(ui: &egui::Ui, cluster: &str, rect: Rect) {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::paint_cluster(ui, cluster, rect);
}

/// Builds an editor layout without changing character offsets. Emoji glyphs
/// are transparent and returned for painting over.
pub fn editor_job(
    text: &str,
    format: &egui::TextFormat,
) -> (egui::text::LayoutJob, Vec<(usize, usize, String)>) {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::editor_job(text, format)
}

/// Paints the emoji over a galley's placeholders.
pub fn paint(ui: &egui::Ui, galley: &egui::Galley, origin: Pos2, placements: &[String]) {
    setup(crate::settings::EmojiRenderer::System);
    fastframe_emoji::paint(ui, galley, origin, placements);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled font alone, as a Mac or Windows machine falls back to it.
    fn bundled() -> fastframe_emoji::Emoji {
        fastframe_emoji::EmojiSetup::default()
            .system(false)
            .bundled(BUNDLED)
            .load()
    }

    #[test]
    fn selecting_noto_excludes_system_emoji_sources() {
        let emoji = setup_for(crate::settings::EmojiRenderer::Noto).load();
        let debug = format!("{emoji:?}");
        assert!(
            debug.contains("sources: [\"bundled\"]"),
            "Noto mode uses only bundled Noto: {debug}"
        );
    }

    #[test]
    fn system_choice_keeps_bundled_noto_as_a_fallback() {
        let emoji = setup_for(crate::settings::EmojiRenderer::System).load();
        assert!(format!("{emoji:?}").contains("bundled"));
    }

    #[test]
    fn the_bundled_font_renders_colour_emoji() {
        let picture = bundled().render("\u{1F600}", 72).expect("grinning face");
        assert_eq!(picture.size[1], 72);
        assert!(picture.has_colour());
    }

    #[test]
    fn the_bundled_font_joins_sequences() {
        let emoji = bundled();
        for sequence in ["🇩🇪", "👍🏽", "👨‍👩‍👧", "#️⃣", "🏳️‍🌈"]
        {
            let joined = emoji.render(sequence, 72).expect("a picture");
            let first: String = sequence.chars().take(1).collect();
            assert_ne!(
                Some(joined),
                emoji.render(&first, 72),
                "{sequence} fell back to its first part"
            );
        }
    }

    #[test]
    fn placeholders_line_up_with_placements() {
        let mut job = LayoutJob::default();
        let mut placements = Vec::new();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            append(
                ui,
                &mut job,
                &mut placements,
                "a 😀 b",
                &TextFormat::default(),
            );
        });
        output.textures_delta.clear();
        assert!(available(), "the bundled font is always there");
        assert_eq!(placements, vec!["😀".to_owned()]);
        assert_eq!(job.text.matches(PLACEHOLDER).count(), 1);
    }

    #[test]
    fn an_editor_job_keeps_the_text_and_places_the_emoji() {
        let format =
            egui::TextFormat::simple(egui::FontId::proportional(14.0), egui::Color32::WHITE);
        let text = "hi 😊 and 👍🏽!";
        let (job, clusters) = editor_job(text, &format);
        assert_eq!(job.text, text, "the galley must mirror the buffer");
        assert_eq!(clusters.len(), 2);
        for (start, length, cluster) in &clusters {
            assert_eq!(
                text.chars().skip(*start).take(*length).collect::<String>(),
                *cluster
            );
        }
    }

    #[test]
    fn emoji_only_messages_are_counted() {
        assert_eq!(only_emoji("😂"), Some(1));
        assert_eq!(only_emoji("😂 🙏"), Some(2));
        assert_eq!(only_emoji("ok 😂"), None);
    }
}
