eliOS: a tour
=============

eliOS is an operating system written from scratch in Rust: its own UEFI
bootloader, kernel, drivers, file system and userland. Its programs speak the
Linux system call ABI, so they're ordinary Linux binaries too.

Things to try
-------------

  ls /                     what's on the (in-memory) file system
  ls -l /bin               every command is a link to one program, eliutils
  ps                       running processes
  fetch                    system information
  free; uptime; date       memory, uptime, the clock
  dmesg | tail             the end of the kernel's log
  echo hi > note.txt       files you create live in memory
  cat note.txt | wc -c     pipes
  seq 100 | grep 7 | wc -l
  sleep 10 &               a background job (then: jobs, wait)
  yes | head -n 3          programs in a pipeline stop when they're done
  sh demo.sh               run a shell script

Keys
----

  Tab                      complete commands and file names
  Up / Down                walk the command history
  Ctrl+C                   interrupt the running program
  Ctrl+L                   clear the screen
  Ctrl+D                   end of input (at the prompt: leave the shell;
                           init starts a fresh one)

When you're done: poweroff (or reboot).
