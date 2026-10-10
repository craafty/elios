//! Build and run eliOS. Invoked as `cargo xtask <command>`.

mod image;
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
  session      boot, type the given steps at the shell, print a transcript
               (steps: `command`, `@raw TEXT`, `@sleep S`, `@key NAME`,
               `@screenshot FILE.png`; see xtask/src/session.rs)

options:
  --release    build with optimizations
  --headless   no QEMU window; serial output only (run; test is always headless)
  --gdb        wait for a debugger on localhost:1234 (run, test)
  --trace      log interrupts and CPU resets to target/qemu.log (run, test)
  --fresh      start from an empty disk image instead of updating it (run)
  --no-kvm     emulate the CPU even if hardware virtualization is available
";

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
        Some("build") => build(&options).map(|_| ()),
        Some("run") => build(&options).and_then(|disk| run(&disk, &options)),
        Some("test") => test(&options),
        Some("session") => build(&options).and_then(|disk| {
            let mut qemu = qemu_command(&disk, &Options { qemu_args: Vec::new(), ..options.clone() })?;
            qemu.arg("-no-shutdown");
            session::run(qemu, &options.qemu_args, &workspace_root().join("target/session-monitor.sock"))
        }),
        Some("image") => {
            let options = Options { fresh: true, ..options };
            build(&options).map(|disk| eprintln!("xtask: wrote {}", disk.display()))
        }
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
            "--" => options.qemu_args.extend(args.by_ref()),
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn profile(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

/// Builds everything and puts it on the disk image. Returns the image.
fn build(options: &Options) -> Result<PathBuf, String> {
    let bootloader = build_bootloader(options.release)?;
    cargo(&["build", "--package", "elikernel", "--target", KERNEL_TARGET], options.release, &[])?;
    let kernel = workspace_root().join("target").join(KERNEL_TARGET).join(profile(options.release)).join("elikernel");
    let initrd = build_initrd()?;
    make_disk_image("eliOS.img", &bootloader, &kernel, &initrd, options.fresh)
}

fn test(options: &Options) -> Result<(), String> {
    let bootloader = build_bootloader(options.release)?;
    let kernel = build_test_kernel(options.release)?;
    let initrd = build_initrd()?;
    let disk = make_disk_image("eliOS-test.img", &bootloader, &kernel, &initrd, true)?;

    let mut qemu = qemu_command(&disk, options)?;
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

/// Puts the bootloader, kernel and initrd on the disk image `target/<name>`.
fn make_disk_image(
    name: &str,
    bootloader: &Path,
    kernel: &Path,
    initrd: &Path,
    fresh: bool,
) -> Result<PathBuf, String> {
    let read = |path: &Path| fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()));
    let files = [
        ("EFI/BOOT/BOOTX64.EFI", read(bootloader)?),
        ("EFI/eliOS/kernel.elf", read(kernel)?),
        ("EFI/eliOS/initrd.cpio", read(initrd)?),
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
    symlink("/bin/eliutils", &sbin.join("init"))?;
    build_c_programs(&root.join("user/c"), &rootfs)?;
    add_busybox(&rootfs)?;

    let mut archive = Vec::new();
    let mut inode = 1;
    add_to_cpio(&mut archive, &rootfs, "", &mut inode)?;
    cpio_entry(&mut archive, 0, "TRAILER!!!", 0, &[]);
    let path = root.join("target/initrd.cpio");
    fs::write(&path, archive).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
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
        if kind.is_symlink() {
            let target = fs::read_link(entry.path()).map_err(|e| e.to_string())?;
            cpio_entry(archive, *inode, &name, 0o120777, target.to_string_lossy().as_bytes());
        } else if kind.is_dir() {
            cpio_entry(archive, *inode, &name, 0o040755, &[]);
            add_to_cpio(archive, &entry.path(), &format!("{name}/"), inode)?;
        } else {
            let data = fs::read(entry.path()).map_err(|e| format!("reading {}: {e}", entry.path().display()))?;
            let executable = name.starts_with("bin/") || name.starts_with("usr/bin/") || name.contains(".so");
            let mode = if executable { 0o100755 } else { 0o100644 };
            cpio_entry(archive, *inode, &name, mode, &data);
        }
    }
    Ok(())
}

/// One entry in the cpio "newc" format: a header of 8-digit hex fields, the
/// name, the data, each padded to 4 bytes.
fn cpio_entry(archive: &mut Vec<u8>, inode: u32, name: &str, mode: u32, data: &[u8]) {
    let fields = [inode, mode, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
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

/// The QEMU invocation shared by `run` and `test`.
fn qemu_command(disk: &Path, options: &Options) -> Result<Command, String> {
    let firmware = find_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args(["-machine", "q35", "-m", "256M", "-nic", "none"])
        .arg("-bios")
        .arg(&firmware)
        // On the q35 machine the disk is attached to an AHCI (SATA)
        // controller, as on a real PC.
        .arg("-drive")
        .arg(format!("format=raw,file={}", disk.display()))
        // The serial port is a second console in this terminal. With
        // signal=off, Ctrl+C goes to eliOS instead of stopping QEMU.
        .args(["-chardev", "stdio,id=console,signal=off", "-serial", "chardev:console"])
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

fn find_ovmf() -> Result<PathBuf, String> {
    if let Ok(path) = env::var("OVMF_PATH") {
        return Ok(path.into());
    }
    OVMF_CANDIDATES.iter().map(PathBuf::from).find(|path| path.exists()).ok_or_else(|| {
        "couldn't find OVMF UEFI firmware; install your distribution's `ovmf` package or set OVMF_PATH".into()
    })
}
