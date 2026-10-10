/* A dynamically linked program: the kernel loads glibc's dynamic linker
   (ld-linux-x86-64.so.2), which loads libc.so.6 and then runs this. The
   libraries are the build machine's own, copied into the initrd. */
#include <stdio.h>
#include <gnu/libc-version.h>

int main(void) {
    printf("Hello from a dynamically linked program, using glibc %s!\n", gnu_get_libc_version());
    return 0;
}
