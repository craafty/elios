//! Build and run eliOS. Invoked as `cargo xtask <command>`.

mod bluetooth;
mod camera;
mod handsfree;
mod image;
mod mirror;
mod session;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: cargo xtask <command> [options] [-- <extra qemu args, or session steps>]

commands:
  build        build everything and update the disk image (target/eliOS.img)
  run          build, then boot the disk image in QEMU
  test         boot a test build of the kernel in QEMU and run its tests
  image        build a fresh disk image (for a USB stick or another VM)
  mirror       serve a caching mirror of Alpine's packages on port 8086
               (QEMU reaches it at 10.0.2.2; see xtask/src/mirror.rs)
  session      boot, type the given steps at the shell, print a transcript
               (steps: `command`, `@raw TEXT`, `@sleep S`, `@key NAME`,
               `@screenshot FILE.png`, `@monitor COMMAND`, `@wait TEXT`; see
               xtask/src/session.rs)

options:
  --release    build with optimizations
  --headless   no QEMU window; serial output only (run; test is always headless)
  --gdb        wait for a debugger on localhost:1234 (run, test)
  --trace      log interrupts and CPU resets to target/qemu.log (run, test)
  --fresh      start from an empty disk image instead of updating it, and
               forget the firmware's settings (boot entries) (run)
  --no-kvm     emulate the CPU even if hardware virtualization is available
  --cpus N     give the machine N CPUs (default 4)
  --memory M   give the machine M of memory, like 4G (default 1G)
  --forward H:G  forward TCP port H on this machine to port G in eliOS
  --second-disk  attach target/second-disk.img (an empty 2 GiB NVMe disk,
               kept between runs) to try partitioning and installing
  --installed  boot only target/second-disk.img: the system the installer
               put there, without the live disk
  --usb-stick  attach the disk image as a USB stick instead of a SATA disk
               (as when eliOS is started from a stick on a real PC)
  --big-stick  a USB stick of 4 GiB that lasts between runs (target/big-stick.img),
               the image written over its start each time, as `dd` does
  --slow-stick  like --big-stick, but as slow as a cheap USB 2.0 flash
               stick (QEMU's throttling: about 30 MB/s and 1000 requests a
               second reading, 10 MB/s and 20 requests a second writing)
  --usb-hub    put the USB keyboard (and stick) behind a USB hub
  --login-screen  session: boot to the login screen instead of a root shell
  --audio-wav FILE  record what eliOS plays to FILE.wav (otherwise the
               sound card's output goes nowhere)
";

const DEFAULT_CPUS: usize = 4;
/// The size of the empty disk `--second-disk` attaches.
const SECOND_DISK_BYTES: u64 = 2 << 30;
const BIG_STICK_BYTES: u64 = 4 << 30;

const BOOTLOADER_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
/// User programs are Linux programs: eliOS implements the Linux system call
/// ABI.
const USER_TARGET: &str = "x86_64-unknown-linux-none";

// The commands eliutils provides (shared with eliutils itself).
include!("../../user/utils/src/applets.rs");

/// Where distributions install OVMF, the open-source UEFI firmware for QEMU.
/// QEMU exit statuses from the test kernel's `isa-debug-exit` writes
/// (see kernel/src/test.rs): QEMU exits with `(value << 1) | 1`.
const TESTS_PASSED: i32 = (0x10 << 1) | 1;
const TESTS_FAILED: i32 = (0x11 << 1) | 1;
const TEST_TIMEOUT: Duration = Duration::from_secs(120);

const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/OVMF/OVMF.fd",
    "/usr/share/qemu/OVMF.fd",
    "/usr/share/edk2/x64/OVMF.fd",
    "/usr/share/edk2-ovmf/x64/OVMF.fd",
    "/usr/share/edk2/ovmf/OVMF.fd",
];

#[derive(Default, Clone)]
struct Options {
    release: bool,
    headless: bool,
    gdb: bool,
    trace: bool,
    fresh: bool,
    no_kvm: bool,
    /// None for the default.
    cpus: Option<usize>,
    /// Memory in MiB; None for the default (1 GiB).
    memory: Option<u32>,
    /// TCP ports to forward from the host into eliOS: (host, guest).
    forwards: Vec<(u16, u16)>,
    second_disk: bool,
    /// Boot the second disk alone.
    installed: bool,
    /// The live disk is a USB stick.
    usb_stick: bool,
    /// USB devices go through a hub.
    usb_hub: bool,
    /// The image written over the start of a bigger, lasting stick.
    big_stick: bool,
    /// The stick is throttled to a real flash stick's speed.
    slow_stick: bool,
    /// Sessions start at the login screen, not logged in as root.
    login_screen: bool,
    /// Use a throwaway copy of the firmware's variables (for tests).
    fresh_firmware: bool,
    /// Where to record the sound card's output.
    audio_wav: Option<PathBuf>,
    qemu_args: Vec<String>,
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let command = args.next();
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => return fail(&error),
    };

    let result = match command.as_deref() {
        Some("build") => build(&options, "").map(|_| ()),
        Some("run") => build(&options, "").and_then(|disk| run(&disk, &options)),
        Some("test") => test(&options),
        // Scripted sessions start at a root shell rather than the login
        // screen.
        Some("session") => build(&options, if options.login_screen { "" } else { "autologin=root" }).and_then(|disk| {
            let mut qemu = qemu_command(&disk, &Options { qemu_args: Vec::new(), ..options.clone() })?;
            qemu.arg("-no-shutdown");
            session::run(qemu, &options.qemu_args, &workspace_root().join("target/session-monitor.sock"))
        }),
        Some("image") => {
            let options = Options { fresh: true, ..options };
            build(&options, "").map(|disk| eprintln!("xtask: wrote {}", disk.display()))
        }
        Some("mirror") => mirror::run(workspace_root().join("target/alpine-cache")),
        Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("unknown or missing command\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}

fn fail(error: &str) -> ExitCode {
    eprintln!("xtask: {error}");
    ExitCode::FAILURE
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--release" => options.release = true,
            "--headless" => options.headless = true,
            "--gdb" => options.gdb = true,
            "--trace" => options.trace = true,
            "--fresh" => options.fresh = true,
            "--no-kvm" => options.no_kvm = true,
            "--forward" => {
                let forward = args.next().and_then(|text| {
                    let (host, guest) = text.split_once(':')?;
                    Some((host.parse().ok()?, guest.parse().ok()?))
                });
                options.forwards.push(forward.ok_or("--forward needs HOST_PORT:GUEST_PORT")?);
            }
            "--second-disk" => options.second_disk = true,
            "--installed" => options.installed = true,
            "--usb-stick" => options.usb_stick = true,
            "--usb-hub" => options.usb_hub = true,
            "--big-stick" => {
                options.big_stick = true;
                options.usb_stick = true;
            }
            "--slow-stick" => {
                options.slow_stick = true;
                options.big_stick = true;
                options.usb_stick = true;
            }
            "--login-screen" => options.login_screen = true,
            "--audio-wav" => options.audio_wav = Some(args.next().ok_or("--audio-wav needs a file name")?.into()),
            "--cpus" => {
                let count = args.next().and_then(|count| count.parse().ok()).filter(|&count| (1..=64).contains(&count));
                options.cpus = Some(count.ok_or("--cpus needs a number from 1 to 64")?);
            }
            "--memory" => {
                let mib = args.next().and_then(|text| parse_memory(&text));
                options.memory = Some(mib.ok_or("--memory needs a size like 4G or 2048M")?);
            }
            "--" => options.qemu_args.extend(args.by_ref()),
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

/// A memory size like `4G` or `2048M` (or a number of MiB), in MiB.
fn parse_memory(text: &str) -> Option<u32> {
    let (number, scale) = match text.strip_suffix(['G', 'g']) {
        Some(number) => (number, 1024),
        None => (text.strip_suffix(['M', 'm']).unwrap_or(text), 1),
    };
    number.parse::<u32>().ok().map(|n| n * scale).filter(|&mib| (256..=65536).contains(&mib))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn profile(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

/// Builds everything and puts it on the disk image, with `command_line` as
/// the kernel's command line. Returns the image.
fn build(options: &Options, command_line: &str) -> Result<PathBuf, String> {
    let bootloader = build_bootloader(options.release)?;
    cargo(&["build", "--package", "elikernel", "--target", KERNEL_TARGET], options.release, &[])?;
    let kernel = workspace_root().join("target").join(KERNEL_TARGET).join(profile(options.release)).join("elikernel");
    let initrd = build_initrd()?;
    let image = make_disk_image("eliOS.img", &bootloader, &kernel, &initrd, command_line, options.fresh)?;
    if options.big_stick { big_stick(&image) } else { Ok(image) }
}

/// What writing the image to a 4 GiB USB stick does (`dd`): the image
/// replaces the stick's start, and the rest keeps what was there, such as
/// the data partition a live system makes there (target/big-stick.img;
/// delete it to start again with an empty stick).
fn big_stick(image: &Path) -> Result<PathBuf, String> {
    use std::io::Write;
    let path = workspace_root().join("target/big-stick.img");
    let bytes = fs::read(image).map_err(|e| format!("reading {}: {e}", image.display()))?;
    let mut stick = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    stick.write_all(&bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
    if stick.metadata().map_err(|e| e.to_string())?.len() < BIG_STICK_BYTES {
        stick.set_len(BIG_STICK_BYTES).map_err(|e| format!("sizing {}: {e}", path.display()))?;
    }
    Ok(path)
}

fn test(options: &Options) -> Result<(), String> {
    let bootloader = build_bootloader(options.release)?;
    let kernel = build_test_kernel(options.release)?;
    let initrd = build_initrd()?;
    let disk = make_disk_image("eliOS-test.img", &bootloader, &kernel, &initrd, "", true)?;

    let mut qemu = qemu_command(&disk, &Options { fresh_firmware: true, ..options.clone() })?;
    qemu.args(["-display", "none", "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    let status = run_with_timeout(qemu, TEST_TIMEOUT)?;
    match status.code() {
        Some(TESTS_PASSED) => Ok(()),
        Some(TESTS_FAILED) => Err("kernel tests failed".into()),
        _ => Err(format!("test kernel stopped without reporting a result (QEMU {status})")),
    }
}

fn build_bootloader(release: bool) -> Result<PathBuf, String> {
    cargo(&["build", "--package", "eliboot", "--target", BOOTLOADER_TARGET], release, &[])?;
    Ok(workspace_root().join("target").join(BOOTLOADER_TARGET).join(profile(release)).join("eliboot.efi"))
}

/// Builds the kernel with cfg(test) and returns the path of the binary.
fn build_test_kernel(release: bool) -> Result<PathBuf, String> {
    let stdout = cargo(
        &[
            "test",
            "--no-run",
            "--package",
            "elikernel",
            "--bin",
            "elikernel",
            "--target",
            KERNEL_TARGET,
            "--message-format=json-render-diagnostics",
        ],
        release,
        &[],
    )?;
    // Cargo prints one JSON object per line; the test binary is the artifact
    // built with the test profile.
    stdout
        .lines()
        .filter(|line| line.contains(r#""reason":"compiler-artifact""#) && line.contains(r#""test":true"#))
        .find_map(|line| json_string_field(line, "executable"))
        .map(PathBuf::from)
        .ok_or_else(|| "cargo didn't report a test kernel binary".into())
}

/// Extracts a string field from one line of JSON. Just enough JSON for
/// cargo's output; xtask has no dependencies, so no JSON library.
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let start = json.find(&format!(r#""{field}":""#))? + field.len() + 4;
    let mut value = String::new();
    let mut chars = json[start..].chars();
    loop {
        match chars.next()? {
            '"' => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                c => value.push(c),
            },
            c => value.push(c),
        }
    }
}

/// Runs cargo in the workspace and returns its standard output (diagnostics
/// still go to the terminal).
fn cargo(args: &[&str], release: bool, environment: &[(&str, &str)]) -> Result<String, String> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(workspace_root()).args(args).envs(environment.iter().copied()).stderr(Stdio::inherit());
    if release {
        command.arg("--release");
    }
    let output = command.output().map_err(|e| format!("running cargo: {e}"))?;
    if !output.status.success() {
        return Err(format!("`cargo {}` failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Puts the bootloader, kernel, initrd and kernel command line on the disk
/// image `target/<name>`.
fn make_disk_image(
    name: &str,
    bootloader: &Path,
    kernel: &Path,
    initrd: &Path,
    command_line: &str,
    fresh: bool,
) -> Result<PathBuf, String> {
    let read = |path: &Path| fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()));
    let files = [
        ("EFI/BOOT/BOOTX64.EFI", read(bootloader)?),
        ("EFI/eliOS/kernel.elf", read(kernel)?),
        ("EFI/eliOS/initrd.cpio", read(initrd)?),
        ("EFI/eliOS/cmdline.txt", command_line.as_bytes().to_vec()),
    ];
    let disk = workspace_root().join("target").join(name);
    image::build(&disk, &files, fresh)?;
    Ok(disk)
}

/// Builds the user programs (always optimized and without debug info: they
/// go into the initrd, where size matters).
fn build_userland() -> Result<PathBuf, String> {
    cargo(
        &[
            "build",
            "--package",
            "eliutils",
            "--package",
            "desktop",
            "--package",
            "ui",
            "--package",
            "term",
            "--package",
            "files",
            "--target",
            USER_TARGET,
            // Tier 3 target: the standard library is built from source.
            "-Zbuild-std=core,alloc",
            "-Zbuild-std-features=compiler-builtins-mem",
        ],
        true,
        &[("CARGO_PROFILE_RELEASE_DEBUG", "false"), ("CARGO_PROFILE_RELEASE_STRIP", "debuginfo")],
    )?;
    Ok(workspace_root().join("target").join(USER_TARGET).join("release").join("eliutils"))
}

/// Assembles the root file system in `target/rootfs` (the repository's
/// `rootfs/` directory plus the programs) and packs it into a cpio archive.
fn build_initrd() -> Result<PathBuf, String> {
    let eliutils = build_userland()?;
    let root = workspace_root();
    let rootfs = root.join("target/rootfs");
    if rootfs.exists() {
        fs::remove_dir_all(&rootfs).map_err(|e| format!("clearing {}: {e}", rootfs.display()))?;
    }
    copy_tree(&root.join("rootfs"), &rootfs)?;

    let bin = rootfs.join("bin");
    let sbin = rootfs.join("sbin");
    for directory in [&bin, &sbin] {
        fs::create_dir_all(directory).map_err(|e| format!("creating {}: {e}", directory.display()))?;
    }
    fs::copy(&eliutils, bin.join("eliutils")).map_err(|e| format!("copying eliutils: {e}"))?;
    for (name, _) in APPLETS {
        symlink("eliutils", &bin.join(name))?;
    }
    let eliauth = eliutils.with_file_name("eliauth");
    fs::copy(&eliauth, bin.join("eliauth")).map_err(|e| format!("copying eliauth: {e}"))?;
    for program in ["desktop", "uidemo", "term", "files", "camera", "settings"] {
        let path = eliutils.with_file_name(program);
        fs::copy(&path, bin.join(program)).map_err(|e| format!("copying {program}: {e}"))?;
    }
    for (name, _) in AUTH_APPLETS {
        symlink("eliauth", &bin.join(name))?;
    }
    symlink("/bin/eliutils", &sbin.join("init"))?;
    // Root's home starts out like everyone else's.
    copy_tree(&rootfs.join("etc/skel"), &rootfs.join("root"))?;
    add_test_account(&rootfs)?;
    build_c_programs(&root.join("user/c"), &rootfs)?;
    add_busybox(&rootfs)?;

    let mut archive = Vec::new();
    let mut inode = 1;
    add_to_cpio(&mut archive, &rootfs, "", &mut inode)?;
    cpio_entry(&mut archive, 0, "TRAILER!!!", 0, 0, &[]);
    let path = root.join("target/initrd.cpio");
    fs::write(&path, archive).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

/// The account development images come with, so trying a build (in QEMU
/// or on the laptop) starts at the login screen with someone to log in as:
/// `test`, password `1234`, an administrator. The installer leaves it
/// behind; installed systems get the account made during installation.
const TEST_ACCOUNT: &str =
    "\n# For trying out development builds (password 1234).\nuser test\n    name = Test User\n    admin = yes\n";
/// SHA-512 crypt of "1234" (`openssl passwd -6 -salt elitestaccount 1234`).
const TEST_PASSWORD_HASH: &str =
    "$6$elitestaccount$rPxv86OrC4jpcdoPPbms4lxhgvrXdM3exJG28XbQs/mAbRZ6kpAIztHVg3vnHkTvhkq4Kh4Yej3uqhm2K.ILM/";

fn add_test_account(rootfs: &Path) -> Result<(), String> {
    let append = |path: &Path, text: &str| {
        let mut contents = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        contents.push_str(text);
        fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
    };
    append(&rootfs.join("etc/eli/system.conf"), TEST_ACCOUNT)?;
    set_host_timezone(&rootfs.join("etc/eli/system.conf"))?;
    append(&rootfs.join("etc/shadow"), &format!("test:{TEST_PASSWORD_HASH}:20371:0:99999:7:::\n"))
}

/// Development images keep the time of the machine that built them (their
/// tester is usually nearby): its zone, from where /etc/localtime points.
fn set_host_timezone(config: &Path) -> Result<(), String> {
    let Ok(target) = fs::read_link("/etc/localtime") else { return Ok(()) };
    let target = target.to_string_lossy().into_owned();
    let Some((_, zone)) = target.split_once("zoneinfo/") else { return Ok(()) };
    let contents = fs::read_to_string(config).map_err(|e| format!("reading {}: {e}", config.display()))?;
    let contents = contents.replace("\ntimezone = UTC\n", &format!("\ntimezone = {zone}\n"));
    fs::write(config, contents).map_err(|e| format!("writing {}: {e}", config.display()))
}

/// Compiles the C programs in `user/c` with the host's C compiler, as
/// ordinary Linux executables: they show eliOS running software it wasn't
/// written for. Most are static; `*-dynamic.c` ones are dynamically linked,
/// and the host's dynamic linker and libraries they need are copied into
/// the root file system too. Skipped (with a note) if there's no compiler.
fn build_c_programs(source: &Path, rootfs: &Path) -> Result<(), String> {
    let compiler = env::var("CC").unwrap_or_else(|_| "cc".into());
    if Command::new(&compiler).arg("--version").output().is_err() {
        eprintln!("xtask: no C compiler ({compiler}); skipping the C programs");
        return Ok(());
    }
    let destination = rootfs.join("usr/bin");
    fs::create_dir_all(&destination).map_err(|e| format!("creating {}: {e}", destination.display()))?;
    let mut sources: Vec<PathBuf> = fs::read_dir(source)
        .map_err(|e| format!("reading {}: {e}", source.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "c"))
        .collect();
    sources.sort();
    for file in sources {
        let name = file.file_stem().unwrap().to_string_lossy().into_owned();
        let output = destination.join(&name);
        let dynamic = name.ends_with("-dynamic");
        let mut command = Command::new(&compiler);
        if !dynamic {
            command.arg("-static");
        }
        let status = command
            .args(["-O2", "-s", "-pthread", "-o"])
            .arg(&output)
            .arg(&file)
            .status()
            .map_err(|e| format!("running {compiler}: {e}"))?;
        if !status.success() {
            return Err(format!("compiling {} failed", file.display()));
        }
        if dynamic {
            copy_shared_libraries(&output, rootfs)?;
        }
    }
    Ok(())
}

/// If the build machine has a static BusyBox, puts it in /usr/bin with a
/// link for each of its commands that eliutils doesn't provide: an unmodified
/// Linux program that brings vi, awk, sed, find, tar and many more.
fn add_busybox(rootfs: &Path) -> Result<(), String> {
    let candidates = ["/usr/bin/busybox", "/bin/busybox"];
    let Some(source) = candidates.iter().map(Path::new).find(|path| path.exists()) else {
        return Ok(());
    };
    let Ok(binary) = fs::read(source) else { return Ok(()) };
    if !is_static_executable(&binary) {
        eprintln!("xtask: {} is dynamically linked; skipping it", source.display());
        return Ok(());
    }
    let output = Command::new(source).arg("--list").output().map_err(|e| format!("running busybox: {e}"))?;
    let destination = rootfs.join("usr/bin");
    fs::create_dir_all(&destination).map_err(|e| format!("creating {}: {e}", destination.display()))?;
    fs::write(destination.join("busybox"), &binary).map_err(|e| format!("copying busybox: {e}"))?;
    let ours: Vec<&str> = APPLETS.iter().map(|(name, _)| *name).collect();
    for applet in String::from_utf8_lossy(&output.stdout).lines() {
        let applet = applet.trim();
        let taken = destination.join(applet).exists() || rootfs.join("bin").join(applet).exists();
        if !applet.is_empty() && !ours.contains(&applet) && !taken && applet != "busybox" {
            symlink("busybox", &destination.join(applet))?;
        }
    }
    Ok(())
}

/// Whether an ELF executable needs no dynamic linker (has no PT_INTERP).
fn is_static_executable(binary: &[u8]) -> bool {
    let read = |at: usize, len: usize| binary.get(at..at + len);
    let (Some(offset), Some(size), Some(count)) = (read(32, 8), read(54, 2), read(56, 2)) else { return false };
    let offset = u64::from_le_bytes(offset.try_into().unwrap()) as usize;
    let size = u16::from_le_bytes(size.try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(count.try_into().unwrap()) as usize;
    binary.starts_with(b"\x7fELF")
        && (0..count).all(|index| read(offset + index * size, 4).is_some_and(|kind| kind != [3, 0, 0, 0]))
}

/// Copies the shared libraries (and dynamic linker) a host program uses into
/// the root file system, at the same paths, as reported by `ldd`.
fn copy_shared_libraries(program: &Path, rootfs: &Path) -> Result<(), String> {
    let output = Command::new("ldd").arg(program).output().map_err(|e| format!("running ldd: {e}"))?;
    let listing = String::from_utf8_lossy(&output.stdout);
    for line in listing.lines() {
        // "libc.so.6 => /lib/.../libc.so.6 (0x...)" or "/lib64/ld-linux... (0x...)"
        let path = match line.split_once("=>") {
            Some((_, rest)) => rest.split_whitespace().next(),
            None => line.split_whitespace().next(),
        };
        let Some(path) = path.filter(|path| path.starts_with('/')) else { continue };
        let target = rootfs.join(path.trim_start_matches('/'));
        if target.exists() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap()).map_err(|e| format!("creating {}: {e}", target.display()))?;
        // fs::copy follows symbolic links, so this copies the real file.
        fs::copy(path, &target).map_err(|e| format!("copying {path}: {e}"))?;
    }
    Ok(())
}

fn symlink(target: &str, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(|e| format!("linking {}: {e}", link.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("creating {}: {e}", to.display()))?;
    let entries = fs::read_dir(from).map_err(|e| format!("reading {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|e| format!("copying {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Files whose permissions differ from the usual (0755 for directories and
/// programs, 0644 for everything else). Everything belongs to root.
const SPECIAL_PERMISSIONS: &[(&str, u32)] = &[
    // su, sudo and passwd run as root whoever starts them.
    ("bin/eliauth", 0o4755),
    // Password hashes are for root's eyes only.
    ("etc/shadow", 0o600),
    ("root", 0o700),
    // Anyone may create files here, but only delete their own.
    ("tmp", 0o1777),
];

/// Adds a directory tree to a cpio archive, parents before children, in a
/// stable order.
fn add_to_cpio(archive: &mut Vec<u8>, directory: &Path, prefix: &str, inode: &mut u32) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .map_err(|e| format!("reading {}: {e}", directory.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        *inode += 1;
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |age| age.as_secs() as u32);
        let special = SPECIAL_PERMISSIONS.iter().find(|(path, _)| *path == name).map(|(_, permissions)| *permissions);
        if kind.is_symlink() {
            let target = fs::read_link(entry.path()).map_err(|e| e.to_string())?;
            cpio_entry(archive, *inode, &name, 0o120777, modified, target.to_string_lossy().as_bytes());
        } else if kind.is_dir() {
            cpio_entry(archive, *inode, &name, 0o040000 | special.unwrap_or(0o755), modified, &[]);
            add_to_cpio(archive, &entry.path(), &format!("{name}/"), inode)?;
        } else {
            let data = fs::read(entry.path()).map_err(|e| format!("reading {}: {e}", entry.path().display()))?;
            let executable = name.starts_with("bin/") || name.starts_with("usr/bin/") || name.contains(".so");
            let permissions = special.unwrap_or(if executable { 0o755 } else { 0o644 });
            cpio_entry(archive, *inode, &name, 0o100000 | permissions, modified, &data);
        }
    }
    Ok(())
}

/// One entry in the cpio "newc" format: a header of 8-digit hex fields, the
/// name, the data, each padded to 4 bytes.
/// Everything belongs to root (user and group 0).
fn cpio_entry(archive: &mut Vec<u8>, inode: u32, name: &str, mode: u32, modified: u32, data: &[u8]) {
    let fields = [inode, mode, 0, 0, 1, modified, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
    archive.extend_from_slice(b"070701");
    for field in fields {
        archive.extend_from_slice(format!("{field:08X}").as_bytes());
    }
    archive.extend_from_slice(name.as_bytes());
    archive.push(0);
    archive.resize(archive.len().next_multiple_of(4), 0);
    archive.extend_from_slice(data);
    archive.resize(archive.len().next_multiple_of(4), 0);
}

fn run(disk: &Path, options: &Options) -> Result<(), String> {
    let mut qemu = qemu_command(disk, options)?;
    if options.headless {
        qemu.args(["-display", "none"]);
    }
    let status = qemu.status().map_err(|e| format!("running qemu-system-x86_64: {e}"))?;
    if !status.success() {
        return Err(format!("QEMU exited with {status}"));
    }
    Ok(())
}

/// The network card and QEMU's NAT network, with any port forwards.
fn network(forwards: &[(u16, u16)]) -> String {
    let mut nic = String::from("user,model=e1000e");
    for (host, guest) in forwards {
        nic.push_str(&format!(",hostfwd=tcp:127.0.0.1:{host}-:{guest}"));
    }
    nic
}

/// The QEMU invocation shared by `run` and `test`.
fn qemu_command(disk: &Path, options: &Options) -> Result<Command, String> {
    let mut qemu = Command::new("qemu-system-x86_64");
    let cpus = options.cpus.unwrap_or(DEFAULT_CPUS).to_string();
    // 1 GiB: packages installed on the live system live in memory.
    let memory = format!("{}M", options.memory.unwrap_or(1024));
    qemu.args(["-machine", "q35", "-m", &memory, "-smp", &cpus])
        // An Intel network card on QEMU's built-in NAT network ("user"
        // networking: the guest is 10.0.2.15, the host 10.0.2.2, DNS
        // 10.0.2.3), so eliOS can reach the host and the internet.
        .arg("-nic")
        .arg(network(&options.forwards));
    firmware(&mut qemu, options)?;
    if !options.installed {
        // On the q35 machine the disk is attached to an AHCI (SATA)
        // controller, as on a real PC (or with --usb-stick, to USB, below).
        // It's first in the boot order, so the firmware starts it even if
        // the second disk has eliOS installed (as a PC would with its USB
        // stick put first).
        // A flash stick is slowest at small requests, writes above all
        // (each rewrites a whole erase block): QEMU's throttling limits
        // requests a second as well as bytes.
        let throttle = if options.slow_stick {
            ",throttling.bps-read=31457280,throttling.iops-read=1000,\
             throttling.bps-write=10485760,throttling.iops-write=20"
        } else {
            ""
        };
        qemu.arg("-drive").arg(format!("if=none,id=live,format=raw,file={}{throttle}", disk.display()));
        if !options.usb_stick {
            qemu.args(["-device", "ide-hd,drive=live,bus=ide.0,bootindex=0"]);
        }
    }
    if options.second_disk || options.installed {
        let path = workspace_root().join("target/second-disk.img");
        if !path.exists() {
            let file = fs::File::create(&path).map_err(|e| format!("creating {}: {e}", path.display()))?;
            file.set_len(SECOND_DISK_BYTES).map_err(|e| format!("sizing {}: {e}", path.display()))?;
        }
        // On NVMe, so both disk drivers get used. Alone (--installed), it
        // gets no boot index: given one, QEMU's firmware replaces its boot
        // order with QEMU's list, dropping entries like the one the
        // installer adds, which real PCs keep.
        let boot_index = if options.installed { "" } else { ",bootindex=1" };
        qemu.arg("-drive")
            .arg(format!("if=none,id=second,format=raw,file={}", path.display()))
            .arg("-device")
            .arg(format!("nvme,drive=second,serial=eli-second{boot_index}"));
    }
    // The serial port is a second console in this terminal. With
    // signal=off, Ctrl+C goes to eliOS instead of stopping QEMU.
    qemu.args(["-chardev", "stdio,id=console,signal=off", "-serial", "chardev:console"])
        // A USB controller with a keyboard and mouse. QEMU types into the
        // keyboard added last, so the window's keystrokes arrive over USB.
        // Eight ports of each kind: a keyboard, a mouse, the camera, the
        // Bluetooth controller and USB sticks need more than the four QEMU
        // gives by default.
        .args(["-device", "qemu-xhci,id=xhci,p2=8,p3=8"]);
    // Behind a hub (on root port 1) when asked: QEMU names hub ports 1.1,
    // 1.2 and so on.
    let place = |port: u32| if options.usb_hub { format!(",port=1.{port}") } else { String::new() };
    if options.usb_hub {
        qemu.args(["-device", "usb-hub,bus=xhci.0,port=1"]);
    }
    if options.usb_stick && !options.installed {
        qemu.arg("-device").arg(format!("usb-storage,bus=xhci.0,drive=live,bootindex=0{}", place(1)));
    }
    // Intel HD Audio, as in a laptop, with QEMU's codec that has a speaker
    // and a microphone. What's played goes nowhere unless recorded.
    let audio = match &options.audio_wav {
        Some(path) => format!("wav,id=sound,path={}", path.display()),
        None => String::from("none,id=sound"),
    };
    qemu.args(["-audiodev", &audio, "-device", "ich9-intel-hda", "-device", "hda-micro,audiodev=sound"]);
    // A pretend webcam (see camera.rs), so the video driver gets tested.
    let socket = workspace_root().join(format!("target/camera-{}.sock", std::process::id()));
    qemu.args(camera::start(&socket)?);
    // And a pretend Intel Bluetooth controller (see bluetooth.rs).
    let socket = workspace_root().join(format!("target/bluetooth-{}.sock", std::process::id()));
    qemu.args(bluetooth::start(&socket)?);
    // A USB mouse, so mice get tested; the window's pointer goes to it.
    qemu.arg("-device").arg(format!("usb-mouse,bus=xhci.0{}", place(3)));
    qemu.arg("-device")
        .arg(format!("usb-kbd,bus=xhci.0{}", place(2)))
        // A triple fault should stop the machine, not quietly reboot it.
        .arg("-no-reboot");
    // Hardware virtualization is much faster than emulating the CPU, when
    // the host allows it.
    let kvm = fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok();
    if kvm && !options.no_kvm {
        qemu.args(["-enable-kvm", "-cpu", "host"]);
    }
    if options.gdb {
        qemu.args(["-s", "-S"]);
        eprintln!("xtask: waiting for gdb on localhost:1234");
    }
    if options.trace {
        let log = workspace_root().join("target/qemu.log");
        qemu.args(["-d", "int,cpu_reset", "-D"]).arg(log);
    }
    qemu.args(&options.qemu_args);
    Ok(qemu)
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<ExitStatus, String> {
    let mut child = command.spawn().map_err(|e| format!("running qemu-system-x86_64: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("waiting for QEMU: {e}"))? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// UEFI firmware for QEMU. Where the distribution has OVMF in two parts
/// (code, and a template for its variables), the variables live in
/// `target/ovmf-vars.fd`, so settings like boot entries persist between
/// runs as on a real PC. Otherwise (or for tests, which get a throwaway
/// copy) the single-file firmware keeps them only while QEMU runs.
fn firmware(qemu: &mut Command, options: &Options) -> Result<(), String> {
    if let Some((code, template)) = find_split_ovmf() {
        let name = if options.fresh_firmware { "ovmf-vars-test.fd" } else { "ovmf-vars.fd" };
        let variables = workspace_root().join("target").join(name);
        if options.fresh || options.fresh_firmware || !variables.exists() {
            fs::copy(&template, &variables).map_err(|e| format!("copying {}: {e}", template.display()))?;
        }
        qemu.arg("-drive")
            .arg(format!("if=pflash,format=raw,unit=0,readonly=on,file={}", code.display()))
            .arg("-drive")
            .arg(format!("if=pflash,format=raw,unit=1,file={}", variables.display()));
        return Ok(());
    }
    qemu.arg("-bios").arg(find_ovmf()?);
    Ok(())
}

/// OVMF's code and variable template, where the distribution has them.
fn find_split_ovmf() -> Option<(PathBuf, PathBuf)> {
    if env::var("OVMF_PATH").is_ok() {
        return None;
    }
    const PAIRS: &[(&str, &str)] = &[
        ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
        ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
    ];
    PAIRS
        .iter()
        .map(|(code, variables)| (PathBuf::from(code), PathBuf::from(variables)))
        .find(|(code, variables)| code.exists() && variables.exists())
}

fn find_ovmf() -> Result<PathBuf, String> {
    if let Ok(path) = env::var("OVMF_PATH") {
        return Ok(path.into());
    }
    OVMF_CANDIDATES.iter().map(PathBuf::from).find(|path| path.exists()).ok_or_else(|| {
        "couldn't find OVMF UEFI firmware; install your distribution's `ovmf` package or set OVMF_PATH".into()
    })
}
