//! Convert local notification sounds to bounded PCM WAV for Notification Center.
//! Native delivery owns sound playback, so Focus applies to custom sounds too.
use crate::settings::NotificationSound;
use rodio::Source;
use sha2::{Digest, Sha256};

pub(super) fn prepare(sound: &NotificationSound) -> Result<Option<String>, String> {
    static PREPARING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = PREPARING.lock().unwrap_or_else(|p| p.into_inner());
    let bytes = match sound {
        NotificationSound::None => return Ok(None),
        NotificationSound::System => return Ok(Some("NSUserNotificationDefaultSoundName".into())),
        NotificationSound::Receive => super::RECEIVE.to_vec(),
        NotificationSound::Alert => super::ALERT.to_vec(),
        NotificationSound::Custom(path) => {
            let file = std::fs::File::open(path).map_err(|_| "sound unavailable")?;
            if file.metadata().map_err(|_| "sound unavailable")?.len() > 16 * 1024 * 1024 {
                return Err("sound file exceeds 16 MiB".into());
            }
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "sound unavailable")?;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err("sound file exceeds 16 MiB".into());
            }
            bytes
        }
    };
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let name = format!("zap-fasterk-{hash}.wav");
    let base = directories::BaseDirs::new().ok_or("home unavailable")?;
    let folder = base.home_dir().join("Library/Sounds");
    let path = folder.join(&name);
    if !path.is_file() {
        let wav = pcm_wave(&bytes)?;
        std::fs::create_dir_all(&folder).map_err(|_| "sound directory unavailable")?;
        // Atomic rename keeps a concurrent notification from reading a partial file.
        let staging = folder.join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::write(&staging, wav).map_err(|_| "sound preparation failed")?;
        std::fs::rename(&staging, &path).map_err(|_| "sound preparation failed")?;
    }
    Ok(Some(name))
}

fn pcm_wave(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let decoder = rodio::Decoder::new(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|_| "sound format unsupported")?;
    let rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    if !(8000..=192000).contains(&rate) || !(1..=2).contains(&channels) {
        return Err("sound must use a supported mono or stereo sample rate".into());
    }
    let limit = rate as usize * channels as usize * 30 - 1;
    let samples: Vec<_> = decoder.take(limit + 1).collect();
    if samples.is_empty() || samples.len() > limit {
        return Err("sound must be shorter than 30 seconds".into());
    }
    let length = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(length as usize + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(length + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&length.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtin_sound_converts_to_pcm_without_an_audio_device() {
        let wav = pcm_wave(crate::notify::RECEIVE).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        let decoded = rodio::Decoder::new(std::io::Cursor::new(wav)).unwrap();
        assert!(decoded.count() > 0);
    }
    #[test]
    fn malformed_sound_is_refused_and_silence_needs_no_files() {
        assert!(pcm_wave(b"invalid").is_err());
        assert_eq!(prepare(&NotificationSound::None).unwrap(), None);
    }
}
