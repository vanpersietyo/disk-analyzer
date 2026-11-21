use rayon::prelude::*;
use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::Metadata,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::Command,
};
use base64::Engine as _;
use std::{collections::HashMap, sync::{Arc, Mutex}};
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System, UpdateKind};
use walkdir::WalkDir;

// Other volumes are scanned on their own; skipping these stops "/" from double-counting them.
const SKIP: [&str; 3] = ["/Volumes", "/System/Volumes", "/dev"];
const TOP: usize = 50;
const FOUND: usize = 300;

#[derive(Serialize)]
struct Volume { name: String, mount: String, total: u64, free: u64 }
#[derive(Serialize)]
struct Entry { name: String, path: String, size: u64, is_dir: bool, system: bool }
#[derive(Serialize)]
struct Big { path: String, size: u64, system: bool }
#[derive(Serialize)]
struct Report { entries: Vec<Entry>, largest: Vec<Big>, total: u64 }

// Allocated size on disk (what Finder/df count), not apparent length.
fn disk_size(m: &Metadata) -> u64 { m.blocks() * 512 }

fn skipped(p: &Path) -> bool { SKIP.iter().any(|s| p == Path::new(s)) }

fn under(s: &str, dir: &str) -> bool { s == dir || s.strip_prefix(dir).map_or(false, |r| r.starts_with('/')) }

// Paths macOS owns: deleting them breaks the OS, so they can be hidden from scans and are never deletable.
// "/private" is stripped first so /var/vm and /private/var/vm (a symlink) are treated alike.
// /usr/local, /Library (third-party) and user data are deliberately NOT included.
fn macos_owned(p: &Path) -> bool {
    let full = p.to_string_lossy();
    let s: &str = match full.strip_prefix("/private") {
        Some(r) if r.is_empty() || r.starts_with('/') => r,
        _ => &full,
    };
    const ROOTS: [&str; 12] = [
        "/System", "/bin", "/sbin", "/cores", "/dev", "/etc", "/var/db", "/var/vm", "/var/protected",
        "/Library/Apple", "/Library/Keychains", "/Library/Updates",
    ];
    if ROOTS.iter().any(|d| under(s, d)) { return true; }
    if s.starts_with("/usr/") && !under(s, "/usr/local") { return true; }
    if s.contains("com.apple.") { return true; } // Apple-owned containers, caches and app support, wherever they live
    if let Some(rest) = s.strip_prefix("/Users/") {
        let mut it = rest.splitn(4, '/');
        let (_user, lib, name) = (it.next(), it.next(), it.next());
        if lib == Some("Library") && matches!(name, Some("Keychains" | "Mail" | "Messages" | "Safari" | "Mobile Documents")) {
            return true;
        }
    }
    false
}

fn walk(root: &Path, hide: bool) -> (u64, BinaryHeap<Reverse<(u64, PathBuf)>>) {
    let (mut total, mut heap) = (0, BinaryHeap::new());
    // Unreadable entries (no Full Disk Access) are skipped silently.
    for e in WalkDir::new(root).into_iter().filter_entry(|e| !skipped(e.path()) && !(hide && macos_owned(e.path()))).flatten() {
        if let Ok(m) = e.metadata() {
            if m.is_file() {
                let s = disk_size(&m);
                total += s;
                heap.push(Reverse((s, e.into_path())));
                if heap.len() > TOP { heap.pop(); }
            }
        }
    }
    (total, heap)
}

#[tauri::command]
fn volumes() -> Vec<Volume> {
    Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| {
            let m = d.mount_point();
            m == Path::new("/") || m.starts_with("/Volumes")
        })
        .map(|d| Volume {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            total: d.total_space(),
            free: d.available_space(),
        })
        .collect()
}

#[tauri::command]
async fn analyze(path: String, hide_system: bool) -> Result<Report, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let kids: Vec<PathBuf> = std::fs::read_dir(&path)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| !skipped(p) && !(hide_system && macos_owned(p)))
            .collect();

        let parts: Vec<_> = kids
            .par_iter()
            .map(|p| {
                let is_dir = std::fs::symlink_metadata(p).map_or(false, |m| m.is_dir());
                let (size, heap) = walk(p, hide_system);
                let name = p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                (Entry { name, path: p.to_string_lossy().into_owned(), size, is_dir, system: macos_owned(p) }, heap)
            })
            .collect();

        let mut entries = Vec::new();
        let mut files = Vec::new();
        for (e, h) in parts {
            files.extend(h.into_iter().map(|Reverse(x)| x));
            entries.push(e);
        }
        entries.sort_by(|a, b| b.size.cmp(&a.size));
        files.sort_by(|a, b| b.0.cmp(&a.0));
        files.truncate(TOP);
        let total = entries.iter().map(|e| e.size).sum();
        let largest = files.into_iter().map(|(size, p)| Big { system: macos_owned(&p), path: p.to_string_lossy().into_owned(), size }).collect();
        Ok(Report { entries, largest, total })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn reveal(path: String) {
    let _ = Command::new("open").args(["-R", &path]).spawn();
}

// Case-insensitive file-name search under `path`; keeps only the FOUND largest hits so a query like "a" can't exhaust memory.
#[tauri::command]
async fn search(path: String, query: String, exts: Vec<String>, hide_system: bool) -> Result<Vec<Big>, String> {
    let q = query.trim().to_lowercase();
    let exts: Vec<String> = exts.iter().map(|e| e.trim().trim_start_matches('.').to_lowercase()).filter(|e| !e.is_empty()).collect();
    if q.is_empty() && exts.is_empty() { return Ok(vec![]); }
    tauri::async_runtime::spawn_blocking(move || {
        let kids: Vec<PathBuf> = std::fs::read_dir(&path)
            .map_err(|e| e.to_string())?
            .flatten()
            .map(|e| e.path())
            .filter(|p| !skipped(p) && !(hide_system && macos_owned(p)))
            .collect();
        let parts: Vec<_> = kids
            .par_iter()
            .map(|k| {
                let mut heap = BinaryHeap::new();
                for e in WalkDir::new(k).into_iter().filter_entry(|e| !skipped(e.path()) && !(hide_system && macos_owned(e.path()))).flatten() {
                    let name = e.file_name().to_string_lossy().to_lowercase();
                    if !name.contains(&q) { continue; }
                    if !exts.is_empty()
                        && !Path::new(&name).extension().and_then(|x| x.to_str()).map_or(false, |x| exts.iter().any(|w| w == x))
                    {
                        continue;
                    }
                    if let Ok(m) = e.metadata() {
                        if m.is_file() {
                            heap.push(Reverse((disk_size(&m), e.into_path())));
                            if heap.len() > FOUND { heap.pop(); }
                        }
                    }
                }
                heap
            })
            .collect();
        let mut all: Vec<(u64, PathBuf)> = parts.into_iter().flat_map(|h| h.into_iter().map(|Reverse(x)| x)).collect();
        all.sort_by(|a, b| b.0.cmp(&a.0));
        all.truncate(FOUND);
        Ok(all.into_iter().map(|(size, p)| Big { system: macos_owned(&p), path: p.to_string_lossy().into_owned(), size }).collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

// Permanent delete (no Trash: moving to Trash frees nothing on the same volume).
// The UI shows a confirmation dialog; this guard is the backstop against bad paths.
#[tauri::command]
async fn delete_path(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    let depth = p.components().count().saturating_sub(1); // minus the root "/"
    if !p.is_absolute()
        || depth < 3 // blocks "/", "/Users", home dir itself, volume roots
        || p.components().any(|c| matches!(c, Component::ParentDir))
        || p.starts_with("/usr") && !p.starts_with("/usr/local") // /usr itself (not just its children) is OS-owned
    {
        return Err("Path dilindungi, tidak bisa dihapus".into());
    }
    if macos_owned(p) {
        return Err("File sistem macOS dilindungi, tidak bisa dihapus".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let m = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        // A deletable parent must not take OS-owned children with it (e.g. a folder holding com.apple.* data).
        if m.is_dir() {
            if let Some(bad) = WalkDir::new(&path).into_iter().flatten().find(|e| macos_owned(e.path())) {
                return Err(format!("Berisi file sistem macOS, tidak dihapus: {}", bad.path().display()));
            }
        }
        let r = if m.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        r.map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
struct Proc { pid: u32, name: String, cmd: String, cpu: f32, mem: u64, kind: &'static str }
#[derive(Serialize)]
struct Perf {
    cpu: f32, cores: usize,
    mem_total: u64, mem_used: u64, swap_total: u64, swap_used: u64,
    load: [f64; 3], procs: Vec<Proc>,
}

fn proc_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().with_cpu().with_memory()
        .with_user(UpdateKind::OnlyIfNotSet).with_cmd(UpdateKind::OnlyIfNotSet).with_exe(UpdateKind::OnlyIfNotSet)
}

// "sistem" = bundled with macOS (by program location) or not ours to signal (root/other users, exe unreadable);
// "app" = lives inside a .app bundle; "tool" = everything else we own (CLI, node, homebrew, ...).
fn classify(exe: Option<&Path>, mine: bool) -> &'static str {
    const SYSTEM: [&str; 7] = ["/System/", "/usr/libexec/", "/usr/sbin/", "/usr/bin/", "/bin/", "/sbin/", "/Library/Apple/"];
    match exe.map(|p| p.to_string_lossy()) {
        Some(s) if SYSTEM.iter().any(|d| s.starts_with(d)) => "sistem",
        Some(s) if s.contains(".app/") => "app",
        Some(_) => "tool",
        None if mine => "tool",
        None => "sistem",
    }
}

// One System kept in app state: CPU usage is a delta between two refreshes, so a fresh System per call would always read 0.
#[tauri::command]
fn perf(state: tauri::State<Mutex<System>>) -> Result<Perf, String> {
    let mut sys = state.lock().map_err(|e| e.to_string())?;
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind());
    let la = System::load_average();
    let my_uid = sysinfo::get_current_pid().ok().and_then(|m| sys.process(m)).and_then(|p| p.user_id()).cloned();
    let procs = sys.processes().values().map(|p| Proc {
        kind: classify(p.exe(), my_uid.is_some() && p.user_id() == my_uid.as_ref()),
        pid: p.pid().as_u32(),
        name: p.name().to_string_lossy().into_owned(),
        cmd: p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ").chars().take(300).collect(),
        cpu: p.cpu_usage(),
        mem: p.memory(),
    }).collect();
    Ok(Perf {
        cpu: sys.global_cpu_usage(), cores: sys.cpus().len(),
        mem_total: sys.total_memory(), mem_used: sys.used_memory(),
        swap_total: sys.total_swap(), swap_used: sys.used_swap(),
        load: [la.one, la.five, la.fifteen], procs,
    })
}

// Only this user's own processes can be signalled; pid 0/1 and the app itself are refused.
#[tauri::command]
fn kill_process(pid: u32, force: bool, state: tauri::State<Mutex<System>>) -> Result<(), String> {
    let mut sys = state.lock().map_err(|e| e.to_string())?;
    let me = sysinfo::get_current_pid().map_err(|e| e.to_string())?;
    let target = Pid::from_u32(pid);
    if pid <= 1 || target == me { return Err("Proses ini dilindungi".into()); }
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[target, me]), true, proc_kind());
    let p = sys.process(target).ok_or("Proses sudah tidak ada")?;
    let mine = sys.process(me).and_then(|m| m.user_id());
    if mine.is_none() || p.user_id() != mine {
        return Err("Hanya proses milik user ini yang bisa dihentikan".into());
    }
    if classify(p.exe(), true) == "sistem" {
        return Err("Proses bawaan macOS dilindungi".into());
    }
    let sent = if force { p.kill() } else { p.kill_with(Signal::Term).unwrap_or(false) };
    if sent { Ok(()) } else { Err("Gagal mengirim sinyal".into()) }
}

// ---- Which installed app does a file belong to? (name + icon shown next to the path) ----

#[derive(Clone)]
struct AppEntry { path: PathBuf, name: String, id: String, icon_file: Option<String>, name_forms: Vec<String>, id_forms: Vec<String> }
#[derive(Serialize, Clone)]
struct AppRef { name: String, icon: Option<String> }
#[derive(Default)]
struct AppCache { apps: Option<Vec<AppEntry>>, icons: HashMap<PathBuf, Option<String>> }
type Shared = Arc<Mutex<AppCache>>;

fn norm(s: &str) -> String { s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect() }

fn make_entry(path: PathBuf, name: String, id: String, folder: String, icon_file: Option<String>) -> AppEntry {
    let mut name_forms: Vec<String> = [norm(&name), norm(&folder)].into_iter().filter(|s| !s.is_empty()).collect();
    name_forms.dedup();
    let id_forms = [norm(&id), norm(id.rsplit('.').next().unwrap_or(""))].into_iter().filter(|s| s.len() >= 3).collect();
    AppEntry { path, name, id, icon_file, name_forms, id_forms }
}

fn read_app(path: &Path) -> Option<AppEntry> {
    let v = plist::Value::from_file(path.join("Contents/Info.plist")).ok()?;
    let d = v.as_dictionary()?;
    let s = |k: &str| d.get(k).and_then(|x| x.as_string()).map(str::to_string);
    let folder = path.file_stem()?.to_string_lossy().into_owned();
    let name = s("CFBundleDisplayName").or_else(|| s("CFBundleName")).unwrap_or_else(|| folder.clone());
    Some(make_entry(path.to_path_buf(), name, s("CFBundleIdentifier").unwrap_or_default(), folder, s("CFBundleIconFile")))
}

fn scan_apps() -> Vec<AppEntry> {
    let home = std::env::var("HOME").unwrap_or_default();
    let is_app = |p: &Path| p.extension().map_or(false, |x| x == "app");
    let mut out = Vec::new();
    for root in ["/Applications".to_string(), "/System/Applications".to_string(), format!("{home}/Applications")] {
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let p = e.path();
            if is_app(&p) { out.extend(read_app(&p)); }
            else if p.is_dir() { // one level of subfolders, e.g. /Applications/Utilities
                for e2 in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    if is_app(&e2.path()) { out.extend(read_app(&e2.path())); }
                }
            }
        }
    }
    out
}

// Comparable spellings of one path component: "HUAQ24HBR6.dev.orbstack" -> devorbstack, orbstack; "GoLand2026.1" -> goland.
fn forms(c: &str) -> Vec<String> {
    let mut c = c.trim_start_matches('.').to_string();
    for suf in [".savedState", ".plist"] {
        if let Some(r) = c.strip_suffix(suf) { c = r.to_string(); }
    }
    // Group Containers are prefixed with a 10-character Apple team id.
    let no_team = c.split_once('.').filter(|(h, _)| h.len() == 10 && h.chars().all(|x| x.is_ascii_uppercase() || x.is_ascii_digit())).map(|(_, r)| r.to_string());
    if let Some(r) = no_team { c = r; }
    let drop_version = |s: &str| s.trim_end_matches(|x: char| x.is_ascii_digit() || matches!(x, '.' | '_' | '-')).to_string();
    let last = c.rsplit('.').next().unwrap_or(&c).to_string();
    [norm(&c), norm(&drop_version(&c)), norm(&last), norm(&drop_version(&last))].into_iter().filter(|s| s.len() >= 3).collect()
}

// Only well-known places are interpreted, and only an installed app's name/bundle id can match,
// so unrelated folders (e.g. ~/.claude-mem) never get a wrong logo.
fn owner(path: &str, apps: &[AppEntry]) -> Option<AppEntry> {
    if let Some(i) = path.find(".app/") {
        let bundle = PathBuf::from(&path[..i + 4]);
        return apps.iter().find(|a| a.path == bundle).cloned().or_else(|| read_app(&bundle));
    }
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if comps.len() < 3 || comps[0] != "Users" { return None; }
    let rest = &comps[2..];
    const SECTIONS: [&str; 10] = [
        "Application Support", "Caches", "Containers", "Group Containers", "Logs",
        "Saved Application State", "HTTPStorages", "WebKit", "Application Scripts", "Cookies",
    ];
    const PLAIN: [&str; 9] = ["Library", "Documents", "Desktop", "Downloads", "Movies", "Music", "Pictures", "Public", "Applications"];
    let cand: &[&str] = if rest[0] == "Library" {
        if rest.len() > 2 && SECTIONS.contains(&rest[1]) { &rest[2..rest.len().min(4)] } else { return None }
    } else if matches!(rest[0], ".cache" | ".config") {
        &rest[1..rest.len().min(3)]
    } else if rest[0] == ".local" && rest.len() > 2 {
        &rest[2..rest.len().min(4)] // .local/share/<app>
    } else if PLAIN.contains(&rest[0]) {
        return None;
    } else {
        &rest[..1] // ~/OrbStack, ~/.android, ...
    };
    let cf: Vec<String> = cand.iter().flat_map(|c| forms(c)).collect();
    // Exact first, so Caches/JetBrains/GoLand2026.1 prefers GoLand over a looser "JetBrains..." prefix hit.
    for f in &cf {
        if let Some(a) = apps.iter().find(|a| a.name_forms.contains(f) || a.id_forms.contains(f)) { return Some(a.clone()); }
    }
    for f in cf.iter().filter(|f| f.len() >= 5) {
        if let Some(a) = apps.iter().find(|a| a.name_forms.iter().any(|n| n.starts_with(f.as_str()))) { return Some(a.clone()); }
    }
    None
}

// 64px PNG of the app icon as a data URI, converted with the bundled `sips`; apps without an .icns just show no logo.
fn icon_data_uri(app: &AppEntry) -> Option<String> {
    let f = app.icon_file.as_ref()?;
    let src = app.path.join("Contents/Resources").join(if f.ends_with(".icns") { f.clone() } else { format!("{f}.icns") });
    let dir = std::env::temp_dir().join("diskanalyzer-icons");
    std::fs::create_dir_all(&dir).ok()?;
    let out = dir.join(format!("{}-{}.png", norm(&app.id), norm(&app.name)));
    if !out.exists() {
        let ok = Command::new("sips").args(["-s", "format", "png", "-Z", "64"]).arg(&src).arg("--out").arg(&out)
            .output().map_or(false, |o| o.status.success());
        if !ok { return None; }
    }
    let bytes = std::fs::read(&out).ok()?;
    Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

#[tauri::command]
async fn app_info(paths: Vec<String>, state: tauri::State<'_, Shared>) -> Result<Vec<Option<AppRef>>, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut g = shared.lock().map_err(|e| e.to_string())?;
        if g.apps.is_none() { g.apps = Some(scan_apps()); }
        let apps = g.apps.clone().unwrap_or_default();
        let mut out = Vec::with_capacity(paths.len());
        for p in &paths {
            out.push(owner(p, &apps).map(|a| {
                let icon = g.icons.entry(a.path.clone()).or_insert_with(|| icon_data_uri(&a)).clone();
                AppRef { name: a.name, icon }
            }));
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

// Also switches the native title bar; "auto" hands control back to the system appearance.
#[tauri::command]
fn set_theme(window: tauri::WebviewWindow, theme: String) {
    let t = match theme.as_str() {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None,
    };
    let _ = window.set_theme(t);
}

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(System::new()))
        .manage(Shared::default())
        .invoke_handler(tauri::generate_handler![volumes, analyze, search, reveal, delete_path, perf, kill_process, set_theme, app_info])
        .run(tauri::generate_context!())
        .expect("error while running app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_owned_paths() {
        let yes = [
            "/System/Library/AssetsV2/x.dmg", "/System", "/usr/bin/ls", "/usr/lib/dyld", "/bin/zsh", "/etc/hosts",
            "/private/etc/hosts", "/var/vm/sleepimage", "/private/var/vm/sleepimage", "/private/var/db/x",
            "/Library/Apple/usr/x", "/Users/a/Library/Containers/com.apple.mail/x", "/Users/a/Library/Keychains",
            "/Users/a/Library/Mail/V10", "/Users/a/Library/Safari/History.db",
        ];
        let no = [
            "/usr", "/usr/local/bin/node", "/opt/homebrew/bin/gh", "/Users/a/Library/Caches/JetBrains",
            "/Users/a/Library/Application Support/Claude", "/Users/a/Project/x", "/private/tmp/x", "/var/folders/ab/T/x",
            "/privatefoo/x", "/Applications/Xcode.app", "/Library/Developer/CoreSimulator", "/Volumes/V-GEN/Meetings",
        ];
        for p in yes { assert!(macos_owned(Path::new(p)), "should be protected: {p}"); }
        for p in no { assert!(!macos_owned(Path::new(p)), "should NOT be protected: {p}"); }
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    fn app(name: &str, id: &str) -> AppEntry {
        make_entry(PathBuf::from(format!("/Applications/{name}.app")), name.into(), id.into(), name.into(), None)
    }

    #[test]
    fn files_map_to_the_right_app_or_none() {
        let apps = [app("OrbStack", "dev.kdrag0n.MacVirt"), app("Claude", "com.anthropic.claudefordesktop"), app("Android Studio", "com.google.android.studio")];
        let who = |p: &str| owner(p, &apps).map(|a| a.name);
        let h = "/Users/macbook";
        assert_eq!(who(&format!("{h}/Library/Group Containers/HUAQ24HBR6.dev.orbstack/data/data.img.raw")).as_deref(), Some("OrbStack"));
        assert_eq!(who(&format!("{h}/OrbStack/docker/volumes/x.ibd")).as_deref(), Some("OrbStack"));
        assert_eq!(who(&format!("{h}/Library/Application Support/Claude/vm_bundles/rootfs.img")).as_deref(), Some("Claude"));
        assert_eq!(who(&format!("{h}/.android/avd/orca_pixel.avd/x.qcow2")).as_deref(), Some("Android Studio")); // prefix match
        // must NOT be attributed: similar-looking names, plain data folders, non-home paths
        assert_eq!(who(&format!("{h}/.claude-mem/claude-mem.db")), None);
        assert_eq!(who(&format!("{h}/.cache/huggingface/hub/x")), None);
        assert_eq!(who(&format!("{h}/Documents/Claude/x")), None);
        assert_eq!(who("/opt/homebrew/share/x.img"), None);
        assert_eq!(who("/Volumes/V-GEN/Meetings/a.mp4"), None);
    }
}
