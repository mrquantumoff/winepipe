//! Wine prefix discovery and WinePipe settings I/O.
//!
//! Settings are *read* straight from the prefix's `user.reg` (instant, no
//! Wine startup) and *written* with `wine reg import`, which goes through the
//! wineserver, so a running prefix never has its registry overwritten.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Mirrors the driver's defaults (crates/pe/src/config.rs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub inputs: u32,
    pub outputs: u32,
    pub buffer_size: u32,
    pub fixed_buffer_size: bool,
    pub autoconnect: bool,
    pub int32: bool,
    pub client_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            inputs: 2,
            outputs: 2,
            buffer_size: 256,
            fixed_buffer_size: false,
            autoconnect: true,
            int32: false,
            client_name: "WinePipe".into(),
        }
    }
}

pub const MAX_CHANNELS: u32 = 64;
pub const BUFFER_SIZES: [u32; 10] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registration {
    pub x64: bool,
    pub x86: bool,
}

#[derive(Clone, Debug)]
pub struct Prefix {
    pub path: PathBuf,
    pub registration: Registration,
}

impl Prefix {
    pub fn label(&self) -> String {
        let home = dirs::home_dir().unwrap_or_default();
        let shown = match self.path.strip_prefix(&home) {
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => self.path.display().to_string(),
        };
        if self.registration.x64 || self.registration.x86 {
            format!("{shown}  •  WinePipe installed")
        } else {
            shown
        }
    }
}

pub fn is_prefix(p: &Path) -> bool {
    p.join("system.reg").is_file() && p.join("user.reg").is_file()
}

/// Find Wine prefixes in the usual places. `extra` (e.g. `$WINEPREFIX` or a
/// `--prefix` argument) is always listed first.
pub fn discover(extra: &[PathBuf]) -> Vec<Prefix> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        let p = p.canonicalize().unwrap_or(p);
        if is_prefix(&p) && !found.contains(&p) {
            found.push(p);
        }
    };
    for p in extra {
        add(p.clone());
    }
    if let Some(home) = dirs::home_dir() {
        if let Ok(rd) = std::fs::read_dir(&home) {
            let mut v: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(".wine")))
                .collect();
            v.sort();
            v.into_iter().for_each(&mut add);
        }
        for dir in [".local/share/wineprefixes", "Games", ".local/share/bottles/bottles", ".var/app/com.usebottles.bottles/data/bottles/bottles"] {
            if let Ok(rd) = std::fs::read_dir(home.join(dir)) {
                let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                v.sort();
                v.into_iter().for_each(&mut add);
            }
        }
    }
    let mut prefixes: Vec<Prefix> =
        found.into_iter().map(|path| Prefix { registration: registration(&path), path }).collect();
    // Explicit prefixes first, then installed ones, then the rest.
    let n_extra = extra.iter().filter(|p| is_prefix(p)).count().min(prefixes.len());
    prefixes[n_extra..].sort_by_key(|p| !(p.registration.x64 || p.registration.x86));
    prefixes
}

/// Registry key line for `key` in Wine's .reg file syntax (backslashes doubled).
fn key_header(key: &str) -> String {
    format!("[{}]", key.replace('\\', "\\\\"))
}

/// Lines of the section for `key` in a Wine registry file. Registry files
/// run to several MB with tens of thousands of keys, so the header match is
/// a case-insensitive prefix compare without allocating per line.
fn section<'a>(text: &'a str, key: &str) -> Option<Vec<&'a str>> {
    let header = key_header(key);
    let h = header.as_bytes();
    let mut lines = text.lines();
    lines.find(|l| {
        let l = l.trim_start().as_bytes();
        l.len() >= h.len()
            && l[..h.len()].eq_ignore_ascii_case(h)
            && l.get(h.len()).is_none_or(|&c| c == b' ' || c == b'\t')
    })?;
    Some(lines.take_while(|l| !l.starts_with('[')).collect())
}

pub fn registration(prefix: &Path) -> Registration {
    let Ok(text) = std::fs::read_to_string(prefix.join("system.reg")) else {
        return Registration::default();
    };
    Registration {
        x64: section(&text, "Software\\ASIO\\WinePipe").is_some(),
        x86: section(&text, "Software\\Wow6432Node\\ASIO\\WinePipe").is_some(),
    }
}

/// Undo .reg string escaping (`\\`, `\"`, `\n`, `\xHHHH`).
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('x') => {
                let mut hex = String::new();
                while hex.len() < 4 && it.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                    hex.push(it.next().unwrap());
                }
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Parse the `[Software\WinePipe]` section of a `user.reg`.
pub fn parse_settings(user_reg: &str) -> Settings {
    let mut s = Settings::default();
    let Some(lines) = section(user_reg, "Software\\WinePipe") else { return s };
    for line in lines {
        let Some(rest) = line.strip_prefix('"') else { continue };
        let Some((name, value)) = rest.split_once("\"=") else { continue };
        let dword = value.strip_prefix("dword:").and_then(|v| u32::from_str_radix(v.trim(), 16).ok());
        match (name.to_ascii_lowercase().as_str(), dword) {
            ("inputs", Some(v)) => s.inputs = v.min(MAX_CHANNELS),
            ("outputs", Some(v)) => s.outputs = v.min(MAX_CHANNELS),
            ("preferredbuffersize", Some(v)) => s.buffer_size = v.clamp(16, 8192),
            ("fixedbuffersize", Some(v)) => s.fixed_buffer_size = v != 0,
            ("autoconnect", Some(v)) => s.autoconnect = v != 0,
            ("sampletype", Some(v)) => s.int32 = v == 1,
            ("clientname", None) => {
                if let Some(v) = value.trim().strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                    s.client_name = unescape(v);
                }
            }
            _ => {}
        }
    }
    s
}

pub fn load_settings(prefix: &Path) -> Settings {
    std::fs::read_to_string(prefix.join("user.reg")).map(|t| parse_settings(&t)).unwrap_or_default()
}

/// The `.reg` file `wine reg import` gets for `s`.
pub fn reg_file(s: &Settings) -> String {
    format!(
        "Windows Registry Editor Version 5.00\r\n\r\n\
         [HKEY_CURRENT_USER\\Software\\WinePipe]\r\n\
         \"Inputs\"=dword:{:08x}\r\n\
         \"Outputs\"=dword:{:08x}\r\n\
         \"PreferredBufferSize\"=dword:{:08x}\r\n\
         \"FixedBufferSize\"=dword:{:08x}\r\n\
         \"AutoConnect\"=dword:{:08x}\r\n\
         \"SampleType\"=dword:{:08x}\r\n\
         \"ClientName\"=\"{}\"\r\n",
        s.inputs,
        s.outputs,
        s.buffer_size,
        s.fixed_buffer_size as u32,
        s.autoconnect as u32,
        s.int32 as u32,
        escape(&s.client_name),
    )
}

/// Expand a leading `~/` (the Wine binary field is free text).
pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn wine_command(wine: &str, prefix: &Path) -> Command {
    let mut c = Command::new(wine);
    c.env("WINEPREFIX", prefix).env("WINEDEBUG", "-all");
    c
}

fn output_text(out: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stderr).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stdout));
    s.lines()
        .filter(|l| !l.contains("MESA") && !l.contains("pci id") && !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write `s` into the prefix through `wine reg import`. Blocking.
pub fn save_settings(wine: &str, prefix: &Path, s: &Settings) -> Result<(), String> {
    // reg.exe wants a Windows path; put the file inside the prefix's C: drive.
    let dir = prefix.join("drive_c/windows/temp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot write to {}: {e}", dir.display()))?;
    let file = dir.join("winepipe-settings.reg");
    std::fs::write(&file, reg_file(s)).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    let out = wine_command(wine, prefix)
        .args(["reg", "import", "C:\\windows\\temp\\winepipe-settings.reg"])
        .output()
        .map_err(|e| format!("cannot run {wine}: {e}"));
    let _ = std::fs::remove_file(&file);
    let out = out?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("reg import failed: {}", output_text(&out)))
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name)));
    let repo = std::env::current_exe()
        .ok()
        .and_then(|e| e.ancestors().nth(3).map(|root| root.join("scripts").join(name)));
    let path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).collect::<Vec<_>>())
        .unwrap_or_default();
    let local = dirs::home_dir().map(|h| h.join(".local/bin").join(name));
    beside.into_iter().chain(repo).chain(path).chain(local).find(|p| p.is_file())
}

/// Run `winepipe-register` for the prefix. Blocking.
pub fn register(wine: &str, prefix: &Path) -> Result<String, String> {
    let tool = find_tool("winepipe-register").ok_or("winepipe-register not found (is WinePipe installed?)")?;
    let out = Command::new(&tool)
        .env("WINEPREFIX", prefix)
        .env("WINE", wine)
        .env("WINEDEBUG", "-all")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tool.display()))?;
    let text = output_text(&out);
    if out.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// Remembered Wine binary per prefix (`~/.config/winepipe/wine-binaries`).
pub struct WineBinaries {
    file: Option<PathBuf>,
    entries: Vec<(PathBuf, String)>,
}

impl WineBinaries {
    pub fn load() -> WineBinaries {
        let file = dirs::config_dir().map(|d| d.join("winepipe").join("wine-binaries"));
        let entries = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| {
                t.lines()
                    .filter_map(|l| l.split_once('\t'))
                    .map(|(p, w)| (PathBuf::from(p), w.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        WineBinaries { file, entries }
    }

    pub fn get(&self, prefix: &Path) -> Option<&str> {
        self.entries.iter().find(|(p, _)| p == prefix).map(|(_, w)| w.as_str())
    }

    pub fn set(&mut self, prefix: &Path, wine: &str) {
        self.entries.retain(|(p, _)| p != prefix);
        if wine != "wine" && !wine.is_empty() {
            self.entries.push((prefix.to_path_buf(), wine.to_string()));
        }
        if let Some(f) = &self.file {
            let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
            let text: String = self.entries.iter().map(|(p, w)| format!("{}\t{w}\n", p.display())).collect();
            let _ = std::fs::write(f, text);
        }
    }
}

/// Best guess for the Wine binary that runs `prefix`: remembered choice,
/// else a launcher script / desktop entry that sets this WINEPREFIX, else
/// plain `wine`.
pub fn guess_wine(prefix: &Path, remembered: &WineBinaries) -> String {
    if let Some(w) = remembered.get(prefix) {
        return w.to_string();
    }
    guess_from_launchers(prefix).unwrap_or_else(|| "wine".into())
}

fn guess_from_launchers(prefix: &Path) -> Option<String> {
    let home = dirs::home_dir()?;
    let pstr = prefix.to_string_lossy().into_owned();
    let rel = prefix.strip_prefix(&home).ok().map(|r| r.to_string_lossy().into_owned());
    let mut files: Vec<PathBuf> = Vec::new();
    for d in [home.join(".local/bin"), home.join(".local/share/applications")] {
        if let Ok(rd) = std::fs::read_dir(d) {
            files.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_file()));
        }
    }
    for f in files {
        // Launchers are small scripts / desktop entries; skip binaries.
        if f.metadata().map_or(true, |m| m.len() > 64 * 1024) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let mentions = text.contains(&pstr) || rel.as_ref().is_some_and(|r| text.contains(&format!("$HOME/{r}")));
        if !mentions {
            continue;
        }
        for tok in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
            let tok = tok.replace("$HOME", &home.to_string_lossy());
            if tok.ends_with("/bin/wine") && Path::new(&tok).is_file() {
                return Some(tok);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_REG: &str = "WINE REGISTRY Version 2\n;; All keys relative to \\\\User\\\\S-1-5-21\n\n\
[Software\\\\Wine\\\\Debug] 1730301234\n\"RelayExclude\"=\"x\"\n\n\
[Software\\\\WinePipe] 1730301234\n#time=1db2a\n\"AutoConnect\"=dword:00000000\n\
\"ClientName\"=\"FL \\\"Studio\\\" \\\\ x\"\n\"Inputs\"=dword:00000008\n\"Outputs\"=dword:00000004\n\
\"PreferredBufferSize\"=dword:00000080\n\"SampleType\"=dword:00000001\n\n\
[Software\\\\Wine\\\\X11 Driver] 1730301234\n\"Inputs\"=dword:00000063\n";

    #[test]
    fn parses_user_reg() {
        let s = parse_settings(USER_REG);
        assert_eq!(s.inputs, 8);
        assert_eq!(s.outputs, 4);
        assert_eq!(s.buffer_size, 128);
        assert!(!s.autoconnect);
        assert!(s.int32);
        assert!(!s.fixed_buffer_size);
        assert_eq!(s.client_name, "FL \"Studio\" \\ x");
    }

    #[test]
    fn missing_section_gives_defaults() {
        assert_eq!(parse_settings("WINE REGISTRY Version 2\n"), Settings::default());
    }

    #[test]
    fn reg_file_roundtrips_through_the_parser() {
        let s = Settings { inputs: 16, outputs: 6, buffer_size: 64, fixed_buffer_size: true, autoconnect: false, int32: true, client_name: "a\\b\"c".into() };
        // Turn the import file into user.reg syntax (key path escaped the same way).
        let text = reg_file(&s).replace("[HKEY_CURRENT_USER\\Software\\WinePipe]", "[Software\\\\WinePipe] 1");
        assert_eq!(parse_settings(&text), s);
    }

    #[test]
    fn registration_section_match_is_exact() {
        let t = "[Software\\\\ASIO\\\\WinePipe2] 1\n[Software\\\\Wow6432Node\\\\ASIO\\\\WinePipe] 1\n";
        assert!(section(t, "Software\\ASIO\\WinePipe").is_none());
        assert!(section(t, "Software\\Wow6432Node\\ASIO\\WinePipe").is_some());
    }

    /// Saves through real Wine. Opt-in:
    /// `WINEPIPE_TEST_PREFIX=~/.cache/winepipe-prefix cargo test -p winepipe-settings -- --ignored`
    #[test]
    #[ignore]
    fn saves_through_wine() {
        let Some(prefix) = std::env::var_os("WINEPIPE_TEST_PREFIX").map(PathBuf::from) else { return };
        let s = Settings { inputs: 5, outputs: 7, buffer_size: 512, fixed_buffer_size: true, autoconnect: false, int32: true, client_name: "Test \"Q\"".into() };
        save_settings("wine", &prefix, &s).unwrap();
        let out = wine_command("wine", &prefix).args(["reg", "query", "HKCU\\Software\\WinePipe"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        for want in ["0x5", "0x7", "0x200", "Test \"Q\""] {
            assert!(text.contains(want), "{want} missing in:\n{text}");
        }
        save_settings("wine", &prefix, &Settings::default()).unwrap();
    }

    /// Real-prefix check: `WINEPIPE_TEST_PREFIX=... cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_prefix_scan() {
        let Some(prefix) = std::env::var_os("WINEPIPE_TEST_PREFIX").map(PathBuf::from) else { return };
        let t = std::time::Instant::now();
        let r = registration(&prefix);
        let s = load_settings(&prefix);
        let all = discover(&[]);
        println!("registration {r:?} settings {s:?} prefixes {} in {:?}", all.len(), t.elapsed());
        assert!(r.x64);
    }
}
