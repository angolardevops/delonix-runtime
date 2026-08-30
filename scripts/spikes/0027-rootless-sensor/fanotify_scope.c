#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/fanotify.h>
int main(void){
    int fd = fanotify_init(FAN_CLASS_NOTIF|FAN_REPORT_FID, O_RDONLY);
    if (fd < 0){ printf("no handle: %s\n", strerror(errno)); return 1; }
    struct { const char*n; unsigned f; const char*p; } t[] = {
        {"FAN_MARK_ADD inode (one directory)",      FAN_MARK_ADD,                    "/tmp"},
        {"FAN_MARK_ADD|FAN_MARK_MOUNT",         FAN_MARK_ADD|FAN_MARK_MOUNT,     "/tmp"},
        {"FAN_MARK_ADD|FAN_MARK_FILESYSTEM",    FAN_MARK_ADD|FAN_MARK_FILESYSTEM,"/tmp"},
    };
    for (unsigned i=0;i<3;i++){
        int r = fanotify_mark(fd, t[i].f, FAN_MODIFY|FAN_ONDIR, AT_FDCWD, t[i].p);
        printf("  %-42s %s\n", t[i].n, r==0 ? "OK" : strerror(errno));
    }
    // FAN_OPEN_PERM = a classe que PODE recusar uma escrita
    int r = fanotify_mark(fd, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, "/tmp");
    printf("  %-42s %s\n", "FAN_OPEN_PERM (refuse, not just observe)", r==0?"OK":strerror(errno));
    close(fd); return 0;
}
