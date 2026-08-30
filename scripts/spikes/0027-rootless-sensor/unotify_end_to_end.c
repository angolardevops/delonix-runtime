// End-to-end proof (ADR-0027): can a supervisor with NO privilege observe a
// syscall, attribute it to a PID, and REFUSE it, on a child process?
#define _GNU_SOURCE
#include <stdio.h>
#include <stddef.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>

static int send_fd(int sock, int fd) {
    struct msghdr msg = {0}; char buf[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec io = { .iov_base = (void*)"x", .iov_len = 1 };
    msg.msg_iov=&io; msg.msg_iovlen=1; msg.msg_control=buf; msg.msg_controllen=sizeof(buf);
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level=SOL_SOCKET; c->cmsg_type=SCM_RIGHTS; c->cmsg_len=CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof(int));
    return sendmsg(sock, &msg, 0) < 0 ? -1 : 0;
}
static int recv_fd(int sock) {
    struct msghdr msg = {0}; char m[1], buf[CMSG_SPACE(sizeof(int))] = {0};
    struct iovec io = { .iov_base = m, .iov_len = 1 };
    msg.msg_iov=&io; msg.msg_iovlen=1; msg.msg_control=buf; msg.msg_controllen=sizeof(buf);
    if (recvmsg(sock, &msg, 0) < 0) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    int fd; memcpy(&fd, CMSG_DATA(c), sizeof(int)); return fd;
}

int main(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) { perror("socketpair"); return 1; }

    pid_t pid = fork();
    if (pid == 0) {                       // ---- the WORKLOAD ----
        close(sv[0]);
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        struct sock_filter f[] = {
            BPF_STMT(BPF_LD|BPF_W|BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K, __NR_mkdir, 0, 1),
            BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_USER_NOTIF),
            BPF_STMT(BPF_RET|BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog prog = { .len=4, .filter=f };
        int nfd = syscall(__NR_seccomp, SECCOMP_SET_MODE_FILTER,
                          SECCOMP_FILTER_FLAG_NEW_LISTENER, &prog);
        if (nfd < 0) { fprintf(stderr, "child: seccomp: %s\n", strerror(errno)); _exit(2); }
        send_fd(sv[1], nfd);
        int r = mkdir("/tmp/spike-should-not-exist", 0700);
        printf("  workload: mkdir returned %d (%s)\n", r, r==0?"created":strerror(errno));
        _exit(r == 0 ? 0 : 1);
    }

    close(sv[1]);                          // ---- the SUPERVISOR ----
    int nfd = recv_fd(sv[0]);
    if (nfd < 0) { perror("recv_fd"); return 1; }
    printf("  supervisor: received the notification fd\n");

    struct seccomp_notif *req = calloc(1, sizeof(*req) + 4096);
    struct seccomp_notif_resp *resp = calloc(1, sizeof(*resp) + 4096);
    if (ioctl(nfd, SECCOMP_IOCTL_NOTIF_RECV, req) < 0) { perror("NOTIF_RECV"); return 1; }

    printf("  supervisor: syscall nr=%llu from pid=%u  <- ATTRIBUTED to a process\n",
           (unsigned long long)req->data.nr, req->pid);

    // Read the argument out of the target's memory: this is what allows a
    // decision based on the PATH, not just on the syscall number.
    char path[256] = {0}; char mem[64];
    snprintf(mem, sizeof(mem), "/proc/%u/mem", req->pid);
    FILE *m = fopen(mem, "r");
    if (m && fseek(m, (long)req->data.args[0], SEEK_SET) == 0 && fgets(path, sizeof(path), m))
        printf("  supervisor: read the target's argument: \"%s\"\n", path);
    else
        printf("  supervisor: could NOT read /proc/<pid>/mem (%s)\n", strerror(errno));
    if (m) fclose(m);

    // The honest caveat: reading a POINTER argument out of the target's memory
    // is a race — the target can rewrite the string after we read it. The kernel
    // offers a validity check, and only that makes the read usable at all.
    __u64 id = req->id;
    int valid = ioctl(nfd, SECCOMP_IOCTL_NOTIF_ID_VALID, &id);
    printf("  supervisor: NOTIF_ID_VALID -> %s\n",
           valid == 0 ? "OK (still the same notification)" : strerror(errno));

    resp->id = req->id; resp->error = -EPERM; resp->val = 0; resp->flags = 0;
    if (ioctl(nfd, SECCOMP_IOCTL_NOTIF_SEND, resp) < 0) { perror("NOTIF_SEND"); return 1; }
    printf("  supervisor: answered EPERM  <- REFUSED the operation\n");

    int st; waitpid(pid, &st, 0);
    printf("  result: the workload exited with %d\n", WEXITSTATUS(st));
    return 0;
}
