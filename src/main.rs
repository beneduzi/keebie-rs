use anyhow::{Context, Result};
use clap::Parser;
use evdev::{Device, InputEventKind, Key};
use std::{collections::HashMap, fs, os::{unix::{io::AsRawFd, process::CommandExt}}, path::{Path, PathBuf}, process::{Command, Stdio}, sync::atomic::{AtomicBool, Ordering}, thread, time::Duration};
use tracing::{error, info, warn};

static RELOAD: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_os_t = default_config_dir())]
    config_dir: PathBuf,
    #[arg(long, default_value = "auto")]
    device: String,
    #[arg(long)]
    layer: Option<String>,
    #[arg(long)]
    list_layers: bool,
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn default_config_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Keebie")
}

struct Config { device: String, layer: String }
fn parse_config(text: &str) -> Result<Config> {
    let mut lines = text.lines();
    let device = lines.next().context("config missing device path")?.trim().to_owned();
    let layer = lines.next().context("config missing active layer")?.trim().to_owned();
    anyhow::ensure!(!device.is_empty() && !layer.is_empty(), "config values must not be empty");
    Ok(Config { device, layer })
}
enum Action { Layer(String), Script(String), Python(String), Shell(String) }
fn parse_action(s: &str) -> Action {
    if let Some(v) = s.strip_prefix("layer:") { Action::Layer(v.to_owned()) }
    else if let Some(v) = s.strip_prefix("script:") { Action::Script(v.to_owned()) }
    else if let Some(v) = s.strip_prefix("py:") { Action::Python(v.to_owned()) }
    else { Action::Shell(s.to_owned()) }
}
fn write_config(path: &Path, config: &Config) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{}\n{}\n", config.device, config.layer))?;
    fs::rename(tmp, path)?;
    Ok(())
}
fn layer_path(dir: &Path, name: &str) -> PathBuf { dir.join("layers").join(name) }
fn load_layer(dir: &Path, name: &str) -> Result<HashMap<String, String>> {
    let path = layer_path(dir, name);
    if !path.exists() { fs::write(&path, r#"{"KEY_ESC":"layer:default"}"#)?; }
    Ok(serde_json::from_str::<HashMap<String, String>>(&fs::read_to_string(path)?)?)
}
fn spawn_action(dir: &Path, value: &str) -> Result<()> {
    match parse_action(value) {
        Action::Shell(cmd) => { let mut c = Command::new("sh"); c.args(["-c", &cmd]); detach(c) }
        Action::Script(s) => spawn_script(dir, &s, "bash"),
        Action::Python(s) => spawn_script(dir, &s, "python3"),
        Action::Layer(_) => Ok(()),
    }
}
fn spawn_script(dir: &Path, spec: &str, interpreter: &str) -> Result<()> {
    let mut words = spec.splitn(2, char::is_whitespace);
    let file = words.next().unwrap_or("");
    let rest = words.next().unwrap_or("");
    let path = dir.join("scripts").join(file);
    let quoted_path = format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let command = if rest.is_empty() {
        format!("exec {interpreter} {quoted_path}")
    } else {
        format!("exec {interpreter} {quoted_path} {rest}")
    };
    let mut c = Command::new("sh");
    c.args(["-c", &command]);
    detach(c)
}
fn detach(mut c: Command) -> Result<()> { c.stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit()); unsafe { c.pre_exec(|| { if libc::setsid() == -1 { return Err(std::io::Error::last_os_error()); } Ok(()) }); } c.spawn().context("spawn action")?; Ok(()) }
struct Grabbed(Device);
impl Drop for Grabbed { fn drop(&mut self) { let _ = self.0.ungrab(); } }
fn key_name(k: Key) -> String { format!("{:?}", k) }
extern "C" fn signal_handler(sig: libc::c_int) { if sig == libc::SIGHUP { RELOAD.store(true, Ordering::SeqCst); } else { STOP.store(true, Ordering::SeqCst); } }
fn install_signals() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = signal_handler as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut());
    }
}
fn main() -> Result<()> {
    let args=Args::parse();
    tracing_subscriber::fmt().with_env_filter(&args.log_level).with_writer(std::io::stderr).init();
    install_signals();
    let cfgpath=args.config_dir.join("config");
    let mut cfg=parse_config(&fs::read_to_string(&cfgpath)?)?;
    if args.list_layers { for e in fs::read_dir(args.config_dir.join("layers"))? { let e=e?; if e.path().extension().is_some_and(|x| x=="json") { println!("{}",e.file_name().to_string_lossy()); } } return Ok(()); }
    let chosen=args.layer.unwrap_or_else(|| cfg.layer.clone());
    let device_path=resolve_device(&args.device, &cfg.device);
    let mut active=chosen;
    let mut map=load_layer(&args.config_dir,&active)?;
    while !STOP.load(Ordering::SeqCst) {
        let mut dev=match Device::open(&device_path) { Ok(d)=>d,Err(e)=>{warn!(%e,"device unavailable; retrying"); thread::sleep(Duration::from_secs(1)); continue;} };
        if let Err(e)=dev.grab() { warn!(%e,"cannot grab device; retrying"); thread::sleep(Duration::from_secs(1)); continue; }
        let mut grabbed=Grabbed(dev);
        let fd = grabbed.0.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            let e = std::io::Error::last_os_error();
            warn!(%e, "cannot set device nonblocking; retrying");
            drop(grabbed);
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        info!(path=%device_path.display(),"device grabbed");
        while !STOP.load(Ordering::SeqCst) {
            if RELOAD.swap(false,Ordering::SeqCst) { match fs::read_to_string(&cfgpath).and_then(|s| parse_config(&s).map_err(std::io::Error::other)) { Ok(c)=>{cfg=c; active=cfg.layer.clone(); match load_layer(&args.config_dir,&active){Ok(m)=>map=m,Err(e)=>error!(%e,"layer reload failed")};},Err(e)=>error!(%e,"config reload failed")} }
            match grabbed.0.fetch_events() { Ok(events)=>for ev in events { if let InputEventKind::Key(k)=ev.kind() { if ev.value()==1 { let key=key_name(k); if let Some(action)=map.get(&key) { match parse_action(action) { Action::Layer(name)=>{ active=name; if let Err(e)=load_layer(&args.config_dir,&active).map(|m|map=m) {error!(%e,"layer switch failed");continue;} cfg.layer=active.clone(); if let Err(e)=write_config(&cfgpath,&cfg){error!(%e,"config write failed");} info!(layer=%active,"layer switched"); }, _=>if let Err(e)=spawn_action(&args.config_dir,action){error!(%e,"action spawn failed")} } } } } },Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(30)),Err(e)=>{warn!(%e,"device read failed; reconnecting");break;} }
        }
        drop(grabbed);
        if !STOP.load(Ordering::SeqCst) { thread::sleep(Duration::from_secs(1)); }
    }
    Ok(())
}
fn resolve_device(arg: &str, configured: &str) -> PathBuf { let s=if arg=="auto"{configured}else{arg}; let p=Path::new(s); if p.is_absolute(){p.to_path_buf()}else{Path::new("/dev/input/by-id").join(s)} }

#[cfg(test)] mod tests {
 use super::*;
 #[test] fn config_lines(){let c=parse_config("/dev/input/foo\ndefault.json\n").unwrap();assert_eq!(c.device,"/dev/input/foo");assert_eq!(c.layer,"default.json");}
 #[test] fn actions(){assert!(matches!(parse_action("layer:x"),Action::Layer(_)));assert!(matches!(parse_action("script:x"),Action::Script(_)));assert!(matches!(parse_action("py:x"),Action::Python(_)));assert!(matches!(parse_action("echo hi"),Action::Shell(_)));}
 #[test] fn layer_json(){let m:HashMap<String,String>=serde_json::from_str(r#"{"KEY_KP0":"echo"}"#).unwrap();assert_eq!(m["KEY_KP0"],"echo");}
 #[test] fn key_names(){assert_eq!(key_name(Key::KEY_KP0),"KEY_KP0");assert_eq!(key_name(Key::KEY_A),"KEY_A");}
 #[test] fn atomic_write(){let d=std::env::temp_dir().join(format!("keebie-test-{}",std::process::id()));fs::create_dir_all(&d).unwrap();let p=d.join("config");write_config(&p,&Config{device:"dev".into(),layer:"a.json".into()}).unwrap();assert_eq!(fs::read_to_string(&p).unwrap(),"dev\na.json\n");let _=fs::remove_dir_all(d);}
}
