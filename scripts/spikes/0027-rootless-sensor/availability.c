// Spike (ADR-0027): what a security sensor can open with NO privilege at all.
#define _GNU_SOURCE
#include <stdio.h>
#include <stddef.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/syscall.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sys/prctl.h>
#include <sys/fanotify.h>
#include <sys/inotify.h>

static void r(const char *what, int fd) {
    if (fd >= 0) { printf("  OK       %s\n", what); close(fd); }
    else         { printf("  DENIED   %-46s errno=%s\n", what, strerror(errno)); }
}

int main(void) {
    printf("== fanotify (the basis of a real FIM) ==\n");
    r("fanotify_init FAN_CLASS_NOTIF",
      fanotify_init(FAN_CLASS_NOTIF, O_RDONLY));
    r("fanotify_init FAN_CLASS_CONTENT (bloqueante)",
      fanotify_init(FAN_CLASS_CONTENT, O_RDONLY));
#ifdef FAN_REPORT_FID
    r("fanotify_init FAN_REPORT_FID (unprivileged mode, >=5.13)",
      fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID, O_RDONLY));
#endif
#ifdef FAN_REPORT_PIDFD
    r("fanotify_init FAN_REPORT_FID|FAN_REPORT_PIDFD",
      fanotify_init(FAN_CLASS_NOTIF | FAN_REPORT_FID | FAN_REPORT_PIDFD, O_RDONLY));
#endif

    printf("== inotify (what is left for integrity) ==\n");
    r("inotify_init1", inotify_init1(IN_NONBLOCK));

    printf("== seccomp user-notification (watch syscalls unprivileged) ==\n");
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
        printf("  DENIED   PR_SET_NO_NEW_PRIVS                          errno=%s\n", strerror(errno));
    } else {
        printf("  OK       PR_SET_NO_NEW_PRIVS\n");
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_mkdir, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog prog = { .len = sizeof(f)/sizeof(f[0]), .filter = f };
        int nfd = syscall(__NR_seccomp, SECCOMP_SET_MODE_FILTER,
                          SECCOMP_FILTER_FLAG_NEW_LISTENER, &prog);
        r("seccomp(SET_MODE_FILTER, NEW_LISTENER) -> notif fd", nfd);
    }

    printf("== bpf() directly ==\n");
    union { char raw[128]; } attr;
    memset(&attr, 0, sizeof(attr));
    int b = syscall(__NR_bpf, 5 /*BPF_PROG_LOAD*/, &attr, sizeof(attr));
    r("bpf(BPF_PROG_LOAD) with an empty attr", b);
    return 0;
}
