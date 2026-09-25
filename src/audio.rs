//! Audio capture: one process per source, kept running for the whole life of
//! the app so the meters work before and after a recording too. `parec` takes
//! the mic and the computer audio; `pw-record` takes only the chosen apps when
//! `computer_apps` is set.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// 20 ms of s16le audio.
const CHUNK_BYTES: usize = (RATE / 50 * 2 * CHANNELS) as usize;
/// Three seconds of 20 ms peaks.
pub const HISTORY: usize = 150;
const FLOOR_DB: f64 = -60.0;

struct Inner {
    levels: VecDeque<f32>,
    file: Option<BufWriter<File>>,
    /// While paused the meters keep running but nothing is written.
    paused: bool,
}

#[derive(Clone)]
pub struct Source {
    inner: Arc<Mutex<Inner>>,
}

impl Source {
    /// Starts capturing `device`, a PulseAudio source name such as `@DEFAULT_MONITOR@`.
    pub fn spawn(device: &'static str) -> Self {
        Self::spawn_command(move || parec(device))
    }

    /// The computer audio: everything that plays, or only the apps named in
    /// `computer_apps` in the config file.
    pub fn spawn_computer() -> Self {
        let apps = computer_apps();
        if apps.is_empty() {
            return Self::spawn("@DEFAULT_MONITOR@");
        }
        thread::spawn(move || {
            loop {
                link_apps(&apps);
                thread::sleep(Duration::from_secs(1));
            }
        });
        Self::spawn_command(apps_capture)
    }

    fn spawn_command(command: impl Fn() -> Command + Send + 'static) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            levels: VecDeque::from(vec![0.0; HISTORY]),
            file: None,
            paused: false,
        }));
        let shared = inner.clone();
        thread::spawn(move || {
            loop {
                capture(command(), &shared);
                // The capture exits when the device goes away; try again.
                thread::sleep(Duration::from_secs(1));
            }
        });
        Source { inner }
    }

    /// Tees the raw stream (s16le, RATE, CHANNELS) into `path` from now on.
    pub fn start_recording(&self, path: &Path) -> std::io::Result<()> {
        let file = BufWriter::new(File::create(path)?);
        let mut inner = self.inner.lock().unwrap();
        inner.file = Some(file);
        inner.paused = false;
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.lock().unwrap().paused = paused;
    }

    pub fn stop_recording(&self) {
        if let Some(mut file) = self.inner.lock().unwrap().file.take() {
            let _ = file.flush();
        }
    }

    pub fn levels(&self) -> Vec<f32> {
        self.inner.lock().unwrap().levels.iter().copied().collect()
    }

    /// The loudest of the last `n` peaks, so a short burst is not missed by a slower reader.
    pub fn recent_peak(&self, n: usize) -> f32 {
        let inner = self.inner.lock().unwrap();
        inner
            .levels
            .iter()
            .rev()
            .take(n)
            .copied()
            .fold(0.0, f32::max)
    }
}

fn parec(device: &str) -> Command {
    let mut command = Command::new("parec");
    command.args([
        "--raw",
        "--format=s16le",
        &format!("--rate={RATE}"),
        &format!("--channels={CHANNELS}"),
        "--latency-msec=20",
        "-d",
        device,
    ]);
    command
}

/// The PipeWire node that captures the chosen apps.
const APPS_NODE: &str = "omarchy-meeting-recorder-apps";

/// A capture stream that connects to nothing by itself and runs on the graph's
/// clock even without links, so the track keeps time with the mic: silence
/// while the apps are quiet or closed, their sound once `link_apps` joins them.
fn apps_capture() -> Command {
    let mut command = Command::new("pw-record");
    command.args([
        "-P",
        &format!(
            "{{ node.name={APPS_NODE} node.description=\"Meeting Recorder apps\" \
             node.autoconnect=false node.always-process=true node.want-driver=true \
             node.latency=960/{RATE} }}"
        ),
        "--rate",
        &RATE.to_string(),
        "--channels",
        &CHANNELS.to_string(),
        "--format",
        "s16",
        "-",
    ]);
    command
}

/// The apps to record as the computer audio, from `computer_apps` in the
/// config file, for instance `computer_apps = ["slack"]`. Empty means all of it.
fn computer_apps() -> Vec<String> {
    std::fs::read_to_string(crate::models::config_file())
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "computer_apps").then(|| parse_list(value))
            })
        })
        .unwrap_or_default()
}

/// `["slack", "zoom"]` to its lowercase entries.
fn parse_list(value: &str) -> Vec<String> {
    value
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|entry| entry.trim().trim_matches('"').trim().to_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Joins the output of every playing stream of the chosen apps to the capture
/// node, channel by channel. A stream matches when its application name,
/// binary or node name contains one of `apps`. Links that exist are left
/// alone; links of streams that end go away with them.
fn link_apps(apps: &[String]) {
    let Some(graph) = Command::new("pw-dump")
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out.stdout).ok())
    else {
        return;
    };
    for (from, to) in missing_links(&graph, apps) {
        let _ = Command::new("pw-link")
            .args([from.to_string(), to.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The (output port, input port) pairs still to link, from a `pw-dump` graph.
fn missing_links(graph: &serde_json::Value, apps: &[String]) -> Vec<(u64, u64)> {
    let Some(objects) = graph.as_array() else {
        return Vec::new();
    };
    let of_type = |kind: &'static str| {
        objects
            .iter()
            .filter(move |o| o["type"] == format!("PipeWire:Interface:{kind}"))
    };
    let Some(capture) = of_type("Node")
        .find(|n| n["info"]["props"]["node.name"] == APPS_NODE)
        .and_then(|n| n["id"].as_u64())
    else {
        return Vec::new();
    };
    let sources: Vec<u64> = of_type("Node")
        .filter(|n| {
            let props = &n["info"]["props"];
            props["media.class"] == "Stream/Output/Audio"
                && ["application.name", "application.process.binary", "node.name"]
                    .iter()
                    .filter_map(|key| props[*key].as_str())
                    .any(|name| {
                        let name = name.to_lowercase();
                        apps.iter().any(|app| name.contains(app.as_str()))
                    })
        })
        .filter_map(|n| n["id"].as_u64())
        .collect();
    let port = |p: &serde_json::Value| {
        let props = &p["info"]["props"];
        Some((
            props["node.id"].as_u64()?,
            p["info"]["direction"].as_str()?.to_owned(),
            props["audio.channel"].as_str().unwrap_or("MONO").to_owned(),
            p["id"].as_u64()?,
        ))
    };
    let ports: Vec<_> = of_type("Port").filter_map(port).collect();
    let inputs: Vec<(String, u64)> = ports
        .iter()
        .filter(|(node, direction, _, _)| *node == capture && direction == "input")
        .map(|(_, _, channel, id)| (channel.clone(), *id))
        .collect();
    let linked: Vec<(u64, u64)> = of_type("Link")
        .filter_map(|l| {
            Some((
                l["info"]["output-port-id"].as_u64()?,
                l["info"]["input-port-id"].as_u64()?,
            ))
        })
        .collect();
    let mut missing = Vec::new();
    for (node, direction, channel, id) in &ports {
        if direction != "output" || !sources.contains(node) {
            continue;
        }
        for (input_channel, input) in &inputs {
            let joins = channel == input_channel || !matches!(channel.as_str(), "FL" | "FR");
            if joins && !linked.contains(&(*id, *input)) {
                missing.push((*id, *input));
            }
        }
    }
    missing
}

fn capture(mut command: Command, shared: &Mutex<Inner>) {
    let Ok(mut child) = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = vec![0u8; CHUNK_BYTES];
    // So a crash loses at most a second: flush every second, and push it to
    // the disk itself every half minute in case the machine goes down too.
    let mut chunks: u64 = 0;
    while stdout.read_exact(&mut buf).is_ok() {
        chunks += 1;
        let peak = buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .unwrap_or(0) as f32
            / 32768.0;
        let mut inner = shared.lock().unwrap();
        inner.levels.pop_front();
        inner.levels.push_back(peak);
        if !inner.paused
            && let Some(file) = inner.file.as_mut()
        {
            let _ = file.write_all(&buf);
            if chunks.is_multiple_of(50) {
                let _ = file.flush();
            }
            if chunks.is_multiple_of(1500) {
                let _ = file.get_ref().sync_data();
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Maps a linear peak to 0..1 on a -60 dB..0 dB scale.
pub fn to_meter(peak: f32) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    (1.0 - 20.0 * f64::from(peak).log10() / FLOOR_DB).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::{APPS_NODE, missing_links, parse_list};
    use serde_json::json;

    #[test]
    fn a_list_of_apps_is_read_lowercase() {
        assert_eq!(parse_list(r#" ["Slack", "zoom" ] # calls"#), ["slack", "zoom"]);
        assert!(parse_list("[]").is_empty());
    }

    fn node(id: u64, props: serde_json::Value) -> serde_json::Value {
        json!({ "id": id, "type": "PipeWire:Interface:Node", "info": { "props": props } })
    }

    fn port(id: u64, node: u64, direction: &str, channel: &str) -> serde_json::Value {
        json!({ "id": id, "type": "PipeWire:Interface:Port",
                "info": { "direction": direction, "props": { "node.id": node, "audio.channel": channel } } })
    }

    #[test]
    fn only_the_chosen_apps_are_linked_channel_by_channel() {
        let stream = |name: &str| json!({ "media.class": "Stream/Output/Audio", "application.name": name });
        let graph = json!([
            node(1, json!({ "node.name": APPS_NODE })),
            port(10, 1, "input", "FL"),
            port(11, 1, "input", "FR"),
            node(2, stream("Slack")),
            port(20, 2, "output", "FL"),
            port(21, 2, "output", "FR"),
            node(3, stream("PipeWire ALSA [cliamp]")),
            port(30, 3, "output", "FL"),
            node(4, stream("slack-helper")),
            port(40, 4, "output", "MONO"),
            { "id": 99, "type": "PipeWire:Interface:Link", "info": { "output-port-id": 20, "input-port-id": 10 } },
        ]);
        assert_eq!(missing_links(&graph, &["slack".into()]), [(21, 11), (40, 10), (40, 11)]);
    }

    #[test]
    fn nothing_is_linked_before_the_capture_node_exists() {
        let graph = json!([node(2, json!({ "media.class": "Stream/Output/Audio", "application.name": "Slack" })), port(20, 2, "output", "FL")]);
        assert!(missing_links(&graph, &["slack".into()]).is_empty());
    }
}
