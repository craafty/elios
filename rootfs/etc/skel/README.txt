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
  edit note.txt            write something (^S saves, ^Q quits)
  echo hi > note.txt       files in your home directory live in memory...
  cp note.txt /disk/       ...but /disk is the boot disk: files there last
  cat note.txt | wc -c     pipes
  seq 100 | grep 7 | wc -l
  sleep 10 &               a background job (then: jobs, wait)
  yes | head -n 3          programs in a pipeline stop when they're done
  sh demo.sh               run a shell script
  nproc; cat /proc/cpuinfo every CPU is used
  threadtest               a C program with many threads (pthreads)

Users
-----

  id; whoami               who you are (the live system logs in as root)
  adduser --admin alice    a new account (--admin: may use sudo)
  su - alice               become alice (or: logout, and log in as her)
  sudo whoami              as alice: run something as root
  passwd                   change your password
  ls -l /home              files have owners and permissions
  chmod 600 note.txt       ...which chmod and chown change

Disks
-----

  lsblk; df                disks, partitions, free space
  mount /dev/sdb1 /mnt     look at another disk (ext2 or FAT)
  installer                put eliOS on a disk in this computer, so it
                           starts without the USB stick (as root; it asks
                           before changing anything)

The network
-----------

  ifconfig                 the network card, and its DHCP address
  ping -c 3 10.0.2.2       QEMU's virtual router answers
  host example.com         look up a name (if the host has internet)
  wget -O - http://example.com/
  nc -l 7777 &             a TCP server... then: echo hi | nc 127.0.0.1 7777
  busybox netstat -tn      connections

Keys
----

  Tab                      complete commands and file names
  Up / Down                walk the command history
  Ctrl+C                   interrupt the running program
  Ctrl+L                   clear the screen
  Ctrl+D                   end of input (at the prompt: log out, and
                           get a login prompt)

When you're done: poweroff (or reboot).
