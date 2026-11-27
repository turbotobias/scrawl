//! Every microphone a capture recorded from (`backupMics`), kept beside the mixed `audio.wav` so
//! a wrong pick can be undone. `tracks/` holds `mixed.wav` (best of each second, what the capture
//! used first), one WAV per input on the bundle's clock, and `tracks.json` naming them and the one
//! in use. `scrawl audio <bundle>` lists them and `--use <n|name>` switches: `audio.wav` becomes
//! that track and the transcript, each keyframe's words, `scrawl.json` and `summary.md` are
//! rebuilt. The status menu's **Audio of Last Capture** does the same for the latest capture.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DIR: &str = "tracks";
const INDEX: &str = "tracks.json";
pub const MIXED: &str = "mixed.wav";
pub const MIXED_NAME: &str = "Best of each second";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub name: String,
    pub file: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tracks {
    pub tracks: Vec<Track>,
    /// The file `audio.wav` was copied from.
    pub using: String,
}

/// Keeps `inputs` (name and samples on the bundle's clock) and the `mixed` result in `dir`, when
/// there was more than one to choose from.
pub fn write(dir: &Path, mixed: &[f32], inputs: &[(String, Vec<f32>)]) -> Result<()> {
    if inputs.len() < 2 {
        return Ok(());
    }
    let folder = dir.join(DIR);
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join(MIXED), crate::audio::encode_wav_16k(mixed)?)?;
    let mut tracks = vec![Track {
        name: MIXED_NAME.into(),
        file: MIXED.into(),
    }];
    for (i, (name, samples)) in inputs.iter().enumerate() {
        let mut samples = samples.clone();
        samples.resize(mixed.len(), 0.0);
        let file = format!("{}.wav", i + 1);
        std::fs::write(folder.join(&file), crate::audio::encode_wav_16k(&samples)?)?;
        tracks.push(Track {
            name: name.clone(),
            file,
        });
    }
    let index = Tracks {
        tracks,
        using: MIXED.into(),
    };
    std::fs::write(folder.join(INDEX), serde_json::to_vec_pretty(&index)?)?;
    Ok(())
}

/// The tracks a bundle kept, if it recorded from more than one input.
pub fn read(dir: &Path) -> Option<Tracks> {
    let bytes = std::fs::read(dir.join(DIR).join(INDEX)).ok()?;
    let tracks: Tracks = serde_json::from_slice(&bytes).ok()?;
    let present = tracks
        .tracks
        .iter()
        .all(|track| dir.join(DIR).join(&track.file).is_file());
    present.then_some(tracks)
}

/// The track `pick` names: its number in `scrawl audio`'s list (0 is the mix), its file, or a
/// part of its name.
pub fn find<'a>(tracks: &'a Tracks, pick: &str) -> Option<&'a Track> {
    let pick = pick.trim();
    if let Ok(n) = pick.parse::<usize>() {
        return tracks.tracks.get(n);
    }
    let lower = pick.to_lowercase();
    tracks
        .tracks
        .iter()
        .find(|track| track.file == pick)
        .or_else(|| {
            tracks
                .tracks
                .iter()
                .find(|track| track.name.to_lowercase().contains(&lower))
        })
}

/// Makes the track `pick` names the bundle's audio and rebuilds its words with `transcribe`.
/// Returns the track's name.
pub fn choose_with(
    dir: &Path,
    pick: &str,
    transcribe: fn(
        Option<&Path>,
        Option<&crate::audio::RecordedAudio>,
    ) -> crate::model::TranscriptData,
) -> Result<String> {
    let mut index = read(dir)
        .context("This capture recorded from one microphone, so there's no other track")?;
    let track = find(&index, pick)
        .with_context(|| format!("No track {pick:?}; `scrawl audio` lists them"))?
        .clone();
    let bytes = std::fs::read(dir.join(DIR).join(&track.file))?;
    let (dir, mut bundle) = crate::handoff::load_bundle(dir)?;
    let wav = dir.join(bundle.audio.clone().unwrap_or_else(|| "audio.wav".into()));
    std::fs::write(&wav, &bytes)?;
    let audio = crate::audio::recorded_audio_from_wav(bytes)?;
    let transcript = transcribe(Some(&wav), Some(&audio));
    crate::speech_model::rewrite_transcript(&dir, &mut bundle, transcript)?;
    index.using = track.file.clone();
    std::fs::write(
        dir.join(DIR).join(INDEX),
        serde_json::to_vec_pretty(&index)?,
    )?;
    crate::capture::scrawl_log(format!("audio: {} now uses {}", dir.display(), track.name));
    Ok(track.name)
}

pub fn choose(dir: &Path, pick: &str) -> Result<String> {
    choose_with(dir, pick, crate::transcribe::transcribe)
}

/// `scrawl audio`'s list: one line per track, the one in use marked.
pub fn describe(tracks: &Tracks) -> String {
    tracks
        .tracks
        .iter()
        .enumerate()
        .map(|(i, track)| {
            let mark = if track.file == tracks.using { "*" } else { " " };
            format!("{mark} {i}  {}\n", track.name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_are_kept_only_when_there_was_a_choice_and_found_by_number_or_name() {
        let dir = std::env::temp_dir().join(format!("scrawl-tracks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mixed = vec![0.1; 1_600];
        write(
            &dir,
            &mixed,
            &[("MacBook Pro Microphone".into(), vec![0.1; 1_600])],
        )
        .unwrap();
        assert_eq!(read(&dir), None, "one input: nothing to choose");

        let inputs = [
            ("MacBook Pro Microphone".to_owned(), vec![0.1; 1_600]),
            ("Tobias’s AirPods Pro".to_owned(), vec![0.0; 800]),
        ];
        write(&dir, &mixed, &inputs).unwrap();
        let tracks = read(&dir).unwrap();
        assert_eq!(tracks.using, MIXED);
        assert_eq!(tracks.tracks.len(), 3);
        // Every track runs on the bundle's clock, as long as the mix.
        let airpods = std::fs::read(dir.join(DIR).join("2.wav")).unwrap();
        assert_eq!(crate::audio::decode_wav_16k(&airpods).unwrap().len(), 1_600);
        assert_eq!(find(&tracks, "0").unwrap().file, MIXED);
        assert_eq!(find(&tracks, "2").unwrap().name, "Tobias’s AirPods Pro");
        assert_eq!(find(&tracks, "airpods").unwrap().file, "2.wav");
        assert_eq!(find(&tracks, "9"), None);
        assert_eq!(
            describe(&tracks).lines().next(),
            Some(&*format!("* 0  {MIXED_NAME}"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
