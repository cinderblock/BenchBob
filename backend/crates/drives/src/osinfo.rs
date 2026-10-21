//! What is installed on a drive, and how it boots (osinfo.ts).
//!
//! Works only from what is mounted (read-only) under the files root, as the service user,
//! without sudo. Paths inside a drive resolve as if the drive were `/`: a Linux rootfs is
//! full of absolute symlinks (etc/os-release -> /usr/lib/os-release), and following them
//! literally would read the host's files instead of the drive's.
//!
//! Cross-references are the useful part, so a whole disk is analysed at once:
//! - a Windows BCD entry stores the partition GUID it boots   -> which partition
//! - a GRUB stub on the ESP does `search.fs_uuid <uuid>`     -> which partition
//! - a Raspberry Pi bootfs has `root=PARTUUID=...`           -> which partition
//!
//! Synchronous file reads: run it on a blocking thread.

use std::path::{Path, PathBuf};

use benchbob_core::http::js_string;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::regf::{Data, Hive, utf16, with_hive};
use crate::util::locale_cmp;

const ESP_TYPES: [&str; 2] = ["c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "0xef"];

fn part_role(t: &str) -> Option<&'static str> {
    Some(match t {
        "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" | "0xef" => "EFI System",
        "e3c9e316-0b5c-4db8-817d-f92df00215ae" => "Microsoft reserved",
        "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7" => "Basic data",
        "de94bba4-06d1-4d40-a16a-bfd50179d6ac" => "Windows recovery",
        "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3" => "Windows LDM metadata",
        "af9b60a0-1431-4f62-bc68-3311714a69ad" => "Windows LDM data",
        "0fc63daf-8483-4772-8e79-3d69d8477de4" => "Linux filesystem",
        "4f68bce3-e8cd-4db1-96e7-fbcaf984b709" => "Linux root (x86-64)",
        "b921b045-1df0-41c3-af44-4c6f280d3fae" => "Linux root (ARM64)",
        "44479540-f297-41b2-9af7-d131d5f0458a" => "Linux root (x86)",
        "933ac7e1-2eb4-4f13-b844-0e14e2aef915" => "Linux /home",
        "bc13c2ff-59e6-4262-a352-b275fd6f7172" => "Linux extended boot",
        "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f" => "Linux swap",
        "e6d6d379-f507-44c2-a23c-238f2a3df928" => "Linux LVM",
        "a19d880f-05fc-4d3b-a006-743f0f84911e" => "Linux RAID",
        "ca7d7ccb-63ed-4c53-861c-1742536059cc" => "LUKS",
        "21686148-6449-6e6f-744e-656564454649" => "BIOS boot",
        "7c3457ef-0000-11aa-aa11-00306543ecac" => "Apple APFS",
        "48465300-0000-11aa-aa11-00306543ecac" => "Apple HFS+",
        "426f6f74-0000-11aa-aa11-00306543ecac" => "Apple boot",
        "52637672-7900-11aa-aa11-00306543ecac" => "Apple recovery",
        "0x7" => "NTFS / exFAT",
        "0xb" => "FAT32",
        "0xc" => "FAT32 (LBA)",
        "0xe" => "FAT16 (LBA)",
        "0x27" => "Windows recovery (hidden)",
        "0x82" => "Linux swap",
        "0x83" => "Linux",
        "0x8e" => "Linux LVM",
        "0xfd" => "Linux RAID",
        _ => return None,
    })
}

fn fs_note(fs: &str) -> Option<&'static str> {
    Some(match fs {
        "BitLocker" => "BitLocker-encrypted — contents can't be read",
        "crypto_LUKS" => "LUKS-encrypted — contents can't be read",
        "LVM2_member" => "LVM physical volume",
        "linux_raid_member" => "Linux RAID member",
        "zfs_member" => "ZFS pool member",
        "swap" => "Swap space",
        _ => return None,
    })
}

/// Folder name under EFI/ (lower-cased) -> what put it there.
fn efi_vendor(k: &str) -> Option<(&'static str, &'static str)> {
    Some(match k {
        "microsoft" => ("Windows Boot Manager", "os"),
        "ubuntu" => ("Ubuntu (GRUB)", "os"),
        "debian" => ("Debian (GRUB)", "os"),
        "fedora" => ("Fedora (GRUB)", "os"),
        "centos" => ("CentOS (GRUB)", "os"),
        "redhat" => ("Red Hat Enterprise Linux (GRUB)", "os"),
        "rocky" => ("Rocky Linux (GRUB)", "os"),
        "almalinux" => ("AlmaLinux (GRUB)", "os"),
        "opensuse" => ("openSUSE (GRUB)", "os"),
        "sles" => ("SUSE Linux Enterprise (GRUB)", "os"),
        "arch" => ("Arch Linux (GRUB)", "os"),
        "manjaro" => ("Manjaro (GRUB)", "os"),
        "endeavouros" => ("EndeavourOS", "os"),
        "pop" => ("Pop!_OS", "os"),
        "neon" => ("KDE neon (GRUB)", "os"),
        "kali" => ("Kali Linux (GRUB)", "os"),
        "gentoo" => ("Gentoo (GRUB)", "os"),
        "proxmox" => ("Proxmox VE (GRUB)", "os"),
        "nixos" => ("NixOS", "os"),
        "void" => ("Void Linux (GRUB)", "os"),
        "linux" => ("Unified kernel images", "os"),
        "apple" => ("Apple", "os"),
        "systemd" => ("systemd-boot", "bootmanager"),
        "refind" => ("rEFInd", "bootmanager"),
        "grub" => ("GRUB", "bootmanager"),
        "clover" => ("Clover", "bootmanager"),
        "oc" => ("OpenCore", "bootmanager"),
        "boot" => ("Fallback loader", "fallback"),
        "dell" => ("Dell firmware tools", "firmware"),
        "hp" => ("HP firmware tools", "firmware"),
        "lenovo" => ("Lenovo firmware tools", "firmware"),
        "asus" => ("ASUS firmware tools", "firmware"),
        "msi" => ("MSI firmware tools", "firmware"),
        "insydeh2o" => ("Insyde firmware tools", "firmware"),
        "tools" => ("EFI tools", "firmware"),
        _ => return None,
    })
}

fn pe_machine(m: u16) -> String {
    match m {
        0x8664 => "x64".into(),
        0xaa64 => "ARM64".into(),
        0x14c => "x86".into(),
        0x1c2 | 0x1c4 => "ARM32".into(),
        0x5064 => "RISC-V 64".into(),
        0xebc => "EFI byte code".into(),
        _ => format!("machine 0x{m:x}"),
    }
}

const BCD_BOOTMGR: f64 = 0x1010_0002 as f64;
const BCD_OSLOADER: f64 = 0x1020_0003 as f64;
const BCD_TOOL: f64 = 0x1020_0005 as f64;

pub struct OsInfo {
    pub files_root: String,
    /// /sys/class/block (partition start offsets for MBR BCD devices).
    pub sys_block: PathBuf,
}

fn norm(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.to_lowercase(),
        Some(Value::Null) | None => String::new(),
        Some(x) if !benchbob_core::http::truthy(x) => String::new(),
        Some(x) => js_string(x).to_lowercase(),
    }
}

/// `p[k] || null` as a string.
fn opt(p: &Value, k: &str) -> Option<String> {
    p.get(k).filter(|v| benchbob_core::http::truthy(v)).map(js_string)
}

// ==================== Reading files inside a drive ====================

/// Resolve `rel` inside `root` as though `root` were `/`: absolute links restart at the
/// drive root and `..` stops there. Components match case-insensitively when there is no
/// exact match (Windows and EFI paths differ in case between installs).
pub fn resolve_under(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut parts: Vec<String> = rel.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
    parts.reverse();
    let mut cur: Vec<String> = Vec::new();
    let mut hops = 0;
    while let Some(p) = parts.pop() {
        if p == "." {
            continue;
        }
        if p == ".." {
            cur.pop();
            continue;
        }
        let dir = cur.iter().fold(root.to_path_buf(), |a, c| a.join(c));
        let mut name = p.clone();
        let mut st = std::fs::symlink_metadata(dir.join(&name)).ok();
        if st.is_none() {
            let lower = p.to_lowercase();
            let m = std::fs::read_dir(&dir)
                .ok()?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .find(|e| e.to_lowercase() == lower)?;
            name = m;
            st = Some(std::fs::symlink_metadata(dir.join(&name)).ok()?);
        }
        if st.is_some_and(|s| s.file_type().is_symlink()) {
            hops += 1;
            if hops > 40 {
                return None;
            }
            let target = std::fs::read_link(dir.join(&name)).ok()?.to_string_lossy().into_owned();
            if target.starts_with('/') {
                cur.clear();
            }
            for t in target.split('/').filter(|s| !s.is_empty()).rev() {
                parts.push(t.to_string());
            }
            continue;
        }
        cur.push(name);
    }
    Some(cur.iter().fold(root.to_path_buf(), |a, c| a.join(c)))
}

fn read_text(root: &Path, rel: &str, max: u64) -> Option<String> {
    let p = resolve_under(root, rel)?;
    let st = std::fs::metadata(&p).ok()?;
    if !st.is_file() || st.len() > max {
        return None;
    }
    std::fs::read(&p).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn list_names(root: &Path, rel: &str) -> Vec<(String, bool)> {
    let Some(p) = resolve_under(root, rel) else { return vec![] };
    std::fs::read_dir(p)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                .collect()
        })
        .unwrap_or_default()
}

fn pe_arch(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut b = vec![0u8; 4096];
    let mut f = std::fs::File::open(path).ok()?;
    let mut n = 0;
    while n < b.len() {
        let r = f.read(&mut b[n..]).ok()?;
        if r == 0 {
            break;
        }
        n += r;
    }
    let b = &b[..n];
    if n < 0x40 || b[0] != 0x4d || b[1] != 0x5a {
        return None; // "MZ"
    }
    let pe = u32::from_le_bytes(b[0x3c..0x40].try_into().ok()?) as usize;
    if pe + 6 > n || u32::from_le_bytes(b[pe..pe + 4].try_into().ok()?) != 0x4550 {
        return None; // "PE\0\0"
    }
    Some(pe_machine(u16::from_le_bytes(b[pe + 4..pe + 6].try_into().ok()?)))
}

fn parse_key_values(text: &str) -> Map<String, Value> {
    // JavaScript's `.` stops at \r, so a CRLF line doesn't match there either.
    let re = Regex::new(r"^\s*([A-Za-z_][A-Za-z0-9_]*)=([^\r\n\u{2028}\u{2029}]*)$").unwrap();
    let q = Regex::new(r#"^(?:"(.*)"|'(.*)')$"#).unwrap();
    let mut out = Map::new();
    for line in text.split('\n') {
        if let Some(m) = re.captures(line) {
            let v = m[2].trim();
            let v = q.captures(v).map(|c| c.get(1).or(c.get(2)).map_or("", |x| x.as_str()).to_string()).unwrap_or_else(|| v.to_string());
            out.insert(m[1].to_string(), v.into());
        }
    }
    out
}

// ==================== Partition lookups ====================

/// A GPT GUID as stored on disk: the first three groups little-endian.
fn guid_bytes(g: &str) -> Option<Vec<u8>> {
    let h: String = g.chars().filter(|c| !matches!(c, '{' | '}' | '-')).collect();
    if h.len() != 32 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let b: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap()).collect();
    let mut out = vec![b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6]];
    out.extend_from_slice(&b[8..]);
    Some(out)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

impl OsInfo {
    pub fn new(files_root: String) -> Self {
        Self { files_root, sys_block: "/sys/class/block".into() }
    }

    fn part_start(&self, name: &str) -> Option<u64> {
        let t = std::fs::read_to_string(self.sys_block.join(name).join("start")).ok()?;
        benchbob_core::http::parse_int(&t).map(|s| s as u64 * 512)
    }

    /// Which partition a BCD device element points at: rather than decode the device path
    /// (several undocumented layouts), look for the partition's identifiers in it.
    fn bcd_device_partition(&self, blob: &[u8], disk: &Value) -> Option<String> {
        let pttype = disk.get("pttype").and_then(Value::as_str);
        for p in disk.get("children").and_then(Value::as_array).into_iter().flatten() {
            let name = p.get("name").map(js_string).unwrap_or_default();
            let partuuid = opt(p, "partuuid");
            if pttype == Some("gpt") {
                if let Some(g) = partuuid.as_deref().and_then(guid_bytes)
                    && contains(blob, &g)
                {
                    return Some(name);
                }
            } else if pttype == Some("dos")
                && let Some(pu) = partuuid
            {
                let Some(start) = self.part_start(&name) else { continue };
                let head = pu.split('-').next().unwrap_or("");
                let digits: String = head.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                let Ok(sig) = u64::from_str_radix(&digits, 16) else { continue };
                if contains(blob, &start.to_le_bytes()) && contains(blob, &(sig as u32).to_le_bytes()) {
                    return Some(name);
                }
            }
        }
        None
    }

    fn read_bcd(&self, path: &Path, disk: &Value) -> anyhow::Result<Option<Value>> {
        Ok(with_hive(path, |h: &mut Hive| {
            let root = h.root()?;
            let Some(objects) = h.child(Some(&root), "Objects")? else { return Ok(None) };
            let mut objs: Vec<(String, f64, crate::regf::Key)> = Vec::new();
            for o in h.subkeys(&objects)? {
                let d = h.child(Some(&o), "Description")?;
                let ty = h.num(d.as_ref(), "Type")?;
                let els = h.child(Some(&o), "Elements")?;
                if let (Some(ty), Some(els)) = (ty, els) {
                    // A JavaScript Map: a repeated key keeps its first place.
                    let id = o.name.to_lowercase();
                    match objs.iter_mut().find(|x| x.0 == id) {
                        Some(x) => *x = (id, ty, els),
                        None => objs.push((id, ty, els)),
                    }
                }
            }
            let el = |h: &mut Hive, els: &crate::regf::Key, id: &str| -> anyhow::Result<Option<Data>> {
                let c = h.child(Some(els), id)?;
                h.get(c.as_ref(), "Element")
            };
            let mut order: Vec<String> = vec![];
            let mut def: Option<String> = None;
            let mut timeout = Value::Null;
            if let Some((_, _, els)) = objs.iter().find(|o| o.1 == BCD_BOOTMGR).cloned() {
                if let Some(Data::Multi(o)) = el(h, &els, "24000001")? {
                    order = o.iter().map(|s| s.to_lowercase()).collect();
                }
                if let Some(Data::Str(d)) = el(h, &els, "23000003")? {
                    def = Some(d.to_lowercase());
                }
                if let Some(Data::Bytes(b)) = el(h, &els, "25000004")?
                    && b.len() >= 8
                {
                    timeout = benchbob_core::http::num(u64::from_le_bytes(b[..8].try_into()?) as f64);
                }
            }
            let mut entries: Vec<(bool, usize, Value)> = Vec::new();
            let (recovery, wim) = (Regex::new(r"(?i)recovery").unwrap(), Regex::new(r"(?i)\.wim\b").unwrap());
            for (id, ty, els) in &objs {
                if *ty != BCD_OSLOADER && *ty != BCD_TOOL {
                    continue;
                }
                let description = el(h, els, "12000004")?;
                let path = el(h, els, "12000002")?;
                let device = el(h, els, "11000001")?;
                let device_text = match &device {
                    Some(Data::Bytes(b)) => utf16(&b[..b.len() & !1]),
                    _ => String::new(),
                };
                let desc_str = match &description {
                    Some(Data::Str(s)) => s.clone(),
                    Some(Data::Num(n)) => crate::util::js_num_str(*n),
                    Some(Data::Multi(m)) => m.join(","),
                    Some(Data::Bytes(b)) => b.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","),
                    None => "null".into(),
                };
                let kind = if *ty == BCD_TOOL {
                    "tool"
                } else if recovery.is_match(&desc_str) || wim.is_match(&device_text) {
                    "recovery"
                } else {
                    "os"
                };
                let in_menu = order.contains(id);
                let partition = match &device {
                    Some(Data::Bytes(b)) => self.bcd_device_partition(b, disk).map(Value::String).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                let rank = ["os", "recovery", "tool"].iter().position(|k| *k == kind).unwrap_or(0);
                entries.push((
                    in_menu,
                    rank,
                    json!({
                        "description": match &description { Some(Data::Str(s)) => s.clone(), _ => "(unnamed)".into() },
                        "path": match &path { Some(Data::Str(s)) => Value::String(s.clone()), _ => Value::Null },
                        "partition": partition,
                        "kind": kind,
                        "isDefault": def.as_deref() == Some(id.as_str()),
                        "inMenu": in_menu,
                    }),
                ));
            }
            // Menu order first, then the rest (recovery, tools).
            entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            Ok(Some(json!({ "timeout": timeout, "entries": entries.into_iter().map(|e| e.2).collect::<Vec<_>>() })))
        })?
        .flatten())
    }

    fn systemd_boot(&self, root: &Path) -> Value {
        let line = Regex::new(r"^\s*(\S+)\s+([^\r\n]+?)\s*$").unwrap();
        let conf_ext = Regex::new(r"(?i)\.conf$").unwrap();
        let mut entries: Vec<Value> = Vec::new();
        for (name, dir) in list_names(root, "loader/entries") {
            if dir || !name.to_lowercase().ends_with(".conf") {
                continue;
            }
            let Some(text) = read_text(root, &format!("loader/entries/{name}"), 64 * 1024).filter(|t| !t.is_empty()) else { continue };
            let mut kv: Map<String, Value> = Map::new();
            for l in text.split('\n') {
                if let Some(m) = line.captures(l)
                    && !m[1].starts_with('#')
                    && !kv.contains_key(&m[1])
                {
                    kv.insert(m[1].to_string(), m[2].to_string().into());
                }
            }
            let title = kv
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| conf_ext.replace(&name, "").into_owned());
            let version =
                kv.get("version").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| Value::String(s.into())).unwrap_or(Value::Null);
            entries.push(json!({ "title": title, "version": version, "file": name }));
        }
        let conf = read_text(root, "loader/loader.conf", 64 * 1024);
        let def = conf.as_deref().and_then(|c| Regex::new(r"(?m)^\s*default\s+(\S+)").unwrap().captures(c).map(|m| m[1].to_string()));
        if entries.is_empty() && conf.as_deref().is_none_or(str::is_empty) {
            return Value::Null;
        }
        json!({ "default": def, "entries": entries })
    }

    fn file_hash(path: &Path) -> Option<u64> {
        std::fs::read(path).ok().map(|b| xxhash_rust::xxh64::xxh64(&b, 0))
    }

    fn analyse_esp(&self, root: &Path, disk: &Value) -> Value {
        let mut loaders: Vec<(usize, String, Value)> = Vec::new();
        let mut all_efi: Vec<(String, PathBuf)> = Vec::new();
        let boot_re = Regex::new(r"^boot(x64|aa64|ia32|arm|riscv64)\.efi$").unwrap();
        let grub = Regex::new(r"(?:search\.fs_uuid|--fs-uuid)(?:\s+--?\S+)*\s+([0-9A-Fa-f][0-9A-Fa-f-]{7,})").unwrap();
        let kinds = ["os", "bootmanager", "fallback", "firmware", "unknown"];
        for (dname, is_dir) in list_names(root, "EFI") {
            if !is_dir {
                continue;
            }
            let key = dname.to_lowercase();
            let vendor = efi_vendor(&key);
            // Microsoft keeps its loaders one level down, in Boot/.
            let sub = if key == "microsoft" { format!("EFI/{dname}/Boot") } else { format!("EFI/{dname}") };
            let files: Vec<String> =
                list_names(root, &sub).into_iter().filter(|(n, d)| !d && n.to_lowercase().ends_with(".efi")).map(|x| x.0).collect();
            for f in &files {
                if let Some(p) = resolve_under(root, &format!("{sub}/{f}")) {
                    all_efi.push((format!("{}/{f}", &sub[4..]), p));
                }
            }
            let lower: Vec<String> = files.iter().map(|f| f.to_lowercase()).collect();
            let pick = [
                lower.iter().position(|f| f.starts_with("shim")),
                lower.iter().position(|f| f.starts_with("grub")),
                lower.iter().position(|f| f == "bootmgfw.efi"),
                lower.iter().position(|f| f.starts_with("systemd-boot")),
                lower.iter().position(|f| f.starts_with("refind")),
                lower.iter().position(|f| boot_re.is_match(f)),
            ]
            .into_iter()
            .flatten()
            .next()
            .or(if files.is_empty() { None } else { Some(0) });
            let main_path = pick.and_then(|i| resolve_under(root, &format!("{sub}/{}", files[i])));
            let kind = vendor.map_or("unknown", |v| v.1);
            let mut loader = json!({
                "dir": dname,
                "name": vendor.map_or(dname.clone(), |v| v.0.to_string()),
                "kind": kind,
                "files": files,
                "arch": main_path.as_deref().and_then(pe_arch),
                "shim": lower.iter().any(|f| f.starts_with("shim")),
            });
            // A distro's ESP grub.cfg is a stub that finds the real /boot by filesystem UUID.
            if let Some(cfg) = read_text(root, &format!("EFI/{dname}/grub.cfg"), 64 * 1024)
                && let Some(m) = grub.captures(&cfg)
            {
                let uuid = m[1].to_string();
                let part = disk
                    .get("children")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|p| norm(p.get("uuid")) == uuid.to_lowercase())
                    .and_then(|p| p.get("name").cloned());
                loader["target"] = json!({ "uuid": uuid, "partition": part.unwrap_or(Value::Null), "system": null });
            }
            loaders.push((kinds.iter().position(|k| *k == kind).unwrap_or(4), dname, loader));
        }
        loaders.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| locale_cmp(&a.1, &b.1, false, false)));

        // The firmware's last resort, EFI/BOOT/BOOT<arch>.EFI, is almost always a copy of
        // some other loader: say which.
        let fb_re = Regex::new(r"(?i)^boot/boot(x64|aa64|ia32|arm|riscv64)\.efi$").unwrap();
        let mut fallback = Value::Null;
        if let Some(fi) = all_efi.iter().position(|e| fb_re.is_match(&e.0)) {
            let fb = &all_efi[fi];
            let size = std::fs::metadata(&fb.1).ok().map(|m| m.len());
            let h = if size.is_some_and(|s| s > 0) { Self::file_hash(&fb.1) } else { None };
            let mut same_as = Value::Null;
            for (i, o) in all_efi.iter().enumerate() {
                if i == fi || o.0.to_lowercase().starts_with("boot/") || std::fs::metadata(&o.1).ok().map(|m| m.len()) != size {
                    continue;
                }
                if h.is_some() && Self::file_hash(&o.1) == h {
                    same_as = o.0.clone().into();
                    break;
                }
            }
            fallback = json!({ "file": fb.0, "arch": pe_arch(&fb.1), "sameAs": same_as });
        }
        let bcd = resolve_under(root, "EFI/Microsoft/Boot/BCD");
        json!({
            "loaders": loaders.into_iter().map(|l| l.2).collect::<Vec<_>>(),
            "fallback": fallback,
            "windowsBoot": bcd.and_then(|p| self.read_bcd(&p, disk).ok().flatten()).unwrap_or(Value::Null),
            "systemdBoot": self.systemd_boot(root),
        })
    }

    fn analyse_windows(&self, root: &Path) -> Option<Value> {
        let software = resolve_under(root, "Windows/System32/config/SOFTWARE")?;
        let info = with_hive(&software, |h| {
            let r = h.root()?;
            let Some(cv) = h.path(&r, &["Microsoft", "Windows NT", "CurrentVersion"])? else { return Ok(None) };
            let cv = Some(&cv);
            let build = h.str(cv, "CurrentBuild")?.filter(|s| !s.is_empty()).or(h.str(cv, "CurrentBuildNumber")?);
            let ubr = h.num(cv, "UBR")?;
            let mut product = h.str(cv, "ProductName")?.filter(|s| !s.is_empty()).unwrap_or_else(|| "Windows".into());
            // Windows 11 never updated ProductName; the build number tells them apart.
            if build.as_deref().and_then(benchbob_core::http::parse_int).is_some_and(|b| b >= 22000) && product.starts_with("Windows 10") {
                product = format!("Windows 11{}", &product["Windows 10".len()..]);
            }
            let install = h.num(cv, "InstallDate")?.filter(|d| *d != 0.0);
            let version = h.str(cv, "DisplayVersion")?.filter(|s| !s.is_empty()).or(h.str(cv, "ReleaseId")?);
            let build = build.filter(|b| !b.is_empty()).map(|b| match ubr {
                Some(u) => format!("{b}.{}", crate::util::js_num_str(u)),
                None => b,
            });
            Ok(Some(json!({
                "product": product,
                "edition": h.str(cv, "EditionID")?,
                "version": version,
                "build": build,
                "installed": install.map(|d| benchbob_core::http::num(d * 1000.0)),
                "computerName": null,
            })))
        })
        .ok()
        .flatten()
        .flatten();
        let mut info = info?;
        if let Some(system) = resolve_under(root, "Windows/System32/config/SYSTEM") {
            let name = with_hive(&system, |h| {
                let r = h.root()?;
                let sel = h.child(Some(&r), "Select")?;
                let cur = h.num(sel.as_ref(), "Current")?.filter(|c| *c != 0.0).unwrap_or(1.0);
                let set = format!("ControlSet{:0>3}", crate::util::js_num_str(cur));
                let cn = h.path(&r, &[&set, "Control", "ComputerName", "ComputerName"])?;
                h.str(cn.as_ref(), "ComputerName")
            })
            .ok()
            .flatten()
            .flatten();
            info["computerName"] = name.map(Value::String).unwrap_or(Value::Null);
        }
        Some(info)
    }

    fn analyse_linux(root: &Path) -> Option<Value> {
        let text = read_text(root, "etc/os-release", 64 * 1024)
            .or_else(|| read_text(root, "usr/lib/os-release", 64 * 1024))
            .filter(|t| !t.is_empty())?;
        let kv = parse_key_values(&text);
        let g = |k: &str| kv.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let name = g("PRETTY_NAME").or_else(|| {
            let j = [g("NAME"), g("VERSION")].into_iter().flatten().collect::<Vec<_>>().join(" ");
            (!j.is_empty()).then_some(j)
        });
        Some(json!({
            "name": name.unwrap_or_else(|| "Linux".into()),
            "id": g("ID"),
            "version": g("VERSION_ID"),
            "hostname": read_text(root, "etc/hostname", 4096).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()),
        }))
    }

    fn analyse_pi_boot(root: &Path, disk: &Value) -> Option<Value> {
        let names: Vec<String> = list_names(root, "").into_iter().map(|e| e.0.to_lowercase()).collect();
        let start = Regex::new(r"^start\d*\.elf$").unwrap();
        let kernel = Regex::new(r"^kernel(\d|_2712)?.*\.img$").unwrap();
        let firmware = names.iter().any(|n| start.is_match(n) || kernel.is_match(n));
        if !names.iter().any(|n| n == "config.txt") || !firmware {
            return None;
        }
        let cmdline = read_text(root, "cmdline.txt", 64 * 1024);
        let pu = cmdline.as_deref().and_then(|c| Regex::new(r"\broot=PARTUUID=(\S+)").unwrap().captures(c).map(|m| m[1].to_string()));
        let issue = read_text(root, "issue.txt", 64 * 1024);
        let rp = pu.as_ref().and_then(|u| {
            disk.get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|p| norm(p.get("partuuid")) == u.to_lowercase())
                .and_then(|p| p.get("name").cloned())
        });
        Some(json!({
            "image": issue.as_deref().map(|i| i.split('\n').next().unwrap_or("").trim().to_string()).filter(|s| !s.is_empty()),
            "rootPartuuid": pu,
            "rootPartition": rp,
        }))
    }

    fn analyse_mac(root: &Path) -> Option<Value> {
        let plist = read_text(root, "System/Library/CoreServices/SystemVersion.plist", 64 * 1024).filter(|t| !t.is_empty())?;
        let get = |k: &str| {
            Regex::new(&format!(r"<key>{k}</key>\s*<string>([^<]*)</string>"))
                .unwrap()
                .captures(&plist)
                .map(|m| m[1].to_string())
                .filter(|s| !s.is_empty())
        };
        Some(
            json!({ "name": get("ProductName").unwrap_or_else(|| "macOS".into()), "version": get("ProductUserVisibleVersion").or_else(|| get("ProductVersion")) }),
        )
    }

    /// `analyseDisk` on lsblk's JSON for the disk (with its children).
    pub fn analyse(&self, disk: &Value, now_ms: i64) -> Value {
        let mut parts: Vec<Value> = Vec::new();
        let root_prefix = format!("{}/", self.files_root);
        for p in disk.get("children").and_then(Value::as_array).into_iter().flatten() {
            let mp = opt(p, "mountpoint").unwrap_or_default();
            let mounted = mp.starts_with(&root_prefix);
            let fstype = opt(p, "fstype");
            let note = fstype.as_deref().and_then(fs_note);
            let mut info = json!({
                "name": p.get("name").cloned().unwrap_or(Value::Null),
                "role": part_role(&norm(p.get("parttype"))),
                "fstype": fstype,
                "label": opt(p, "label").or_else(|| opt(p, "partlabel")),
                "path": if mounted { Value::String(mp[root_prefix.len()..].to_string()) } else { Value::Null },
                "note": note,
            });
            if mounted {
                let root = PathBuf::from(&mp);
                let is_esp = ESP_TYPES.contains(&norm(p.get("parttype")).as_str())
                    || (fstype.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("vfat")) && resolve_under(&root, "EFI").is_some());
                if is_esp {
                    info["esp"] = self.analyse_esp(&root, disk);
                }
                if let Some(w) = self.analyse_windows(&root) {
                    info["windows"] = w;
                }
                if let Some(l) = Self::analyse_linux(&root) {
                    info["linux"] = l;
                }
                if let Some(pi) = Self::analyse_pi_boot(&root, disk) {
                    info["pi"] = pi;
                }
                if let Some(m) = Self::analyse_mac(&root) {
                    info["macos"] = m;
                }
            } else if note.is_none() && fstype.is_some() && mp.is_empty() {
                info["note"] = "Not mounted — contents not inspected".into();
            }
            parts.push(info);
        }
        // What each GRUB stub ends up booting, now that every partition is known.
        let linux_of: Vec<(Value, Value)> =
            parts.iter().map(|p| (p["name"].clone(), p.get("linux").and_then(|l| l.get("name")).cloned().unwrap_or(Value::Null))).collect();
        for p in &mut parts {
            if let Some(Value::Array(ls)) = p.get_mut("esp").and_then(|e| e.get_mut("loaders")) {
                for l in ls {
                    if let Some(t) = l.get_mut("target")
                        && t.get("partition").is_some_and(benchbob_core::http::truthy)
                    {
                        let sys = linux_of.iter().find(|(n, _)| *n == t["partition"]).map(|x| x.1.clone()).unwrap_or(Value::Null);
                        t["system"] = sys;
                    }
                }
            }
        }
        let systems = summarise(&parts);
        json!({
            "device": disk.get("name").cloned().unwrap_or(Value::Null),
            "partitionTable": opt(disk, "pttype"),
            "partitions": parts,
            "systems": systems,
            "analysedAt": now_ms,
        })
    }
}

fn summarise(parts: &[Value]) -> Vec<Value> {
    let mut systems = Vec::new();
    let esps: Vec<&Value> = parts.iter().filter_map(|p| p.get("esp")).collect();
    let bcd: Vec<&Value> =
        esps.iter().flat_map(|e| e.pointer("/windowsBoot/entries").and_then(Value::as_array).into_iter().flatten()).collect();
    let grub: Vec<&Value> =
        esps.iter().flat_map(|e| e["loaders"].as_array().into_iter().flatten().filter(|l| l.get("target").is_some())).collect();
    let pi_root = |q: &Value| q.pointer("/pi/rootPartition").cloned().unwrap_or(Value::Null);
    let join = |v: Vec<String>, sep: &str| v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(sep);
    for p in parts {
        let name = &p["name"];
        if let Some(w) = p.get("windows") {
            let via: Vec<String> = bcd
                .iter()
                .filter(|e| e["partition"] == *name && e["kind"] == "os")
                .map(|e| format!("Windows Boot Manager: \"{}\"", e["description"].as_str().unwrap_or("")))
                .collect();
            let s = |k: &str| w.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let detail = join(
                vec![
                    if s("build").is_empty() { String::new() } else { format!("build {}", s("build")) },
                    if s("computerName").is_empty() { String::new() } else { format!("PC name {}", s("computerName")) },
                ],
                " · ",
            );
            systems.push(json!({
                "kind": "windows",
                "name": join(vec![s("product"), s("version")], " "),
                "detail": if detail.is_empty() { Value::Null } else { detail.into() },
                "partition": name,
                "bootsVia": via,
            }));
        }
        if let Some(l) = p.get("linux") {
            let mut via: Vec<String> = grub
                .iter()
                .filter(|g| g["target"]["partition"] == *name)
                .map(|g| format!("EFI/{}", g["dir"].as_str().unwrap_or("")))
                .collect();
            for q in parts {
                if pi_root(q) == *name {
                    via.push(format!("Raspberry Pi bootfs ({})", q["name"].as_str().unwrap_or("")));
                }
            }
            let host = l.get("hostname").and_then(Value::as_str).unwrap_or("");
            systems.push(json!({
                "kind": if parts.iter().any(|q| pi_root(q) == *name) { "pi" } else { "linux" },
                "name": l["name"],
                "detail": if host.is_empty() { Value::Null } else { format!("hostname {host}").into() },
                "partition": name,
                "bootsVia": via,
            }));
        }
        if let Some(m) = p.get("macos") {
            let n = join(vec![m["name"].as_str().unwrap_or("").into(), m["version"].as_str().unwrap_or("").into()], " ");
            systems.push(json!({ "kind": "macos", "name": n, "detail": null, "partition": name, "bootsVia": [] }));
        }
    }
    // Boot entries whose OS partition couldn't be read (encrypted, unmounted) still say
    // something is installed.
    for e in &bcd {
        if e["kind"] != "os" || e["inMenu"] != true {
            continue;
        }
        if !e["partition"].is_null() && parts.iter().find(|p| p["name"] == e["partition"]).is_some_and(|p| p.get("windows").is_some()) {
            continue;
        }
        systems.push(json!({
            "kind": "windows", "name": e["description"], "detail": "from the Windows boot menu; partition not readable",
            "partition": e["partition"], "bootsVia": ["Windows Boot Manager"],
        }));
    }
    for p in parts {
        let Some(pi) = p.get("pi") else { continue };
        let rp = &pi["rootPartition"];
        let has_linux = !rp.is_null() && parts.iter().find(|q| q["name"] == *rp).is_some_and(|q| q.get("linux").is_some());
        if rp.is_null() || !has_linux {
            systems.push(json!({
                "kind": "pi",
                "name": pi.get("image").filter(|i| !i.is_null()).cloned().unwrap_or("Raspberry Pi OS".into()),
                "detail": "rootfs not readable",
                "partition": rp,
                "bootsVia": [format!("Raspberry Pi bootfs ({})", p["name"].as_str().unwrap_or(""))],
            }));
        }
    }
    systems
}
