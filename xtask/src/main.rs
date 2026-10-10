//! Build and run eliOS. Invoked as `cargo xtask <command>`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo xtask <command> [options] [-- <extra qemu args>]

commands:
  build        build eliboot and the kernel, and lay out the boot volume
  run          build, then boot eliOS in QEMU

options:
  --release    build with optimizations
  --headless   no QEMU window; serial output only (run)
  --gdb        wait for a debugger on localhost:1234 (run)
  --trace      log interrupts and CPU resets to target/qemu.log (run)
";

const BOOTLOADER_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_TARGET: &str = "x86_64-unknown-none";

/// Where distributions install OVMF, the open-source UEFI firmware for QEMU.
const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/OVMF/OVMF.fd",
    "/usr/share/qemu/OVMF.fd",
    "/usr/share/edk2/x64/OVMF.fd",
    "/usr/share/edk2-ovmf/x64/OVMF.fd",
    "/usr/share/edk2/ovmf/OVMF.fd",
];

#[derive(Default)]
struct Options {
    release: bool,
    headless: bool,
    gdb: bool,
    trace: bool,
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
        Some("run") => build(&options).and_then(|esp| run(&esp, &options)),
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
            "--" => options.qemu_args.extend(args.by_ref()),
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Builds both binaries and copies them into an EFI system partition layout.
/// Returns the partition's directory.
fn build(options: &Options) -> Result<PathBuf, String> {
    let root = workspace_root();
    let profile = if options.release { "release" } else { "debug" };

    cargo_build(&root, "eliboot", BOOTLOADER_TARGET, options.release)?;
    cargo_build(&root, "elikernel", KERNEL_TARGET, options.release)?;

    let target = root.join("target");
    let esp = target.join("esp");
    let files = [
        (target.join(BOOTLOADER_TARGET).join(profile).join("eliboot.efi"), esp.join("EFI/BOOT/BOOTX64.EFI")),
        (target.join(KERNEL_TARGET).join(profile).join("elikernel"), esp.join("EFI/eliOS/kernel.elf")),
    ];
    for (from, to) in &files {
        fs::create_dir_all(to.parent().unwrap()).map_err(|e| format!("creating {}: {e}", to.display()))?;
        fs::copy(from, to).map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
    }
    Ok(esp)
}

fn cargo_build(root: &Path, package: &str, target: &str, release: bool) -> Result<(), String> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(root).args(["build", "--package", package, "--target", target]);
    if release {
        command.arg("--release");
    }
    let status = command.status().map_err(|e| format!("running cargo: {e}"))?;
    if !status.success() {
        return Err(format!("building {package} failed"));
    }
    Ok(())
}

fn run(esp: &Path, options: &Options) -> Result<(), String> {
    let firmware = find_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.args(["-machine", "q35", "-m", "256M", "-nic", "none"])
        .arg("-bios")
        .arg(&firmware)
        // Exposes the directory to the guest as a FAT disk, so there's no
        // image to rebuild while iterating.
        .arg("-drive")
        .arg(format!("format=raw,file=fat:rw:{}", esp.display()))
        .args(["-serial", "stdio"])
        // A triple fault should stop the machine, not quietly reboot it.
        .arg("-no-reboot");
    if options.headless {
        qemu.args(["-display", "none"]);
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

    let status = qemu.status().map_err(|e| format!("running qemu-system-x86_64: {e}"))?;
    if !status.success() {
        return Err(format!("QEMU exited with {status}"));
    }
    Ok(())
}

fn find_ovmf() -> Result<PathBuf, String> {
    if let Ok(path) = env::var("OVMF_PATH") {
        return Ok(path.into());
    }
    OVMF_CANDIDATES.iter().map(PathBuf::from).find(|path| path.exists()).ok_or_else(|| {
        "couldn't find OVMF UEFI firmware; install your distribution's `ovmf` package or set OVMF_PATH".into()
    })
}
