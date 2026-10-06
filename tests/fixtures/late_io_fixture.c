/* Linux-only qualification fixture. Loaded into one disposable test child,
 * never into kei. The immutable child environment names an exclusive root;
 * only regular .part files beneath that root and its directory fsyncs qualify.
 * All forwarding signatures match libc, buffers are bounded, and pthread_once
 * resolves RTLD_NEXT before concurrent calls. No provider data is recorded. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static pthread_once_t init_once = PTHREAD_ONCE_INIT;
static ssize_t (*next_write)(int, const void *, size_t);
static int (*next_fsync)(int);
static int (*next_fdatasync)(int);
static void init_calls(void) {
    next_write = dlsym(RTLD_NEXT, "write");
    next_fsync = dlsym(RTLD_NEXT, "fsync");
    next_fdatasync = dlsym(RTLD_NEXT, "fdatasync");
    if (!next_write || !next_fsync || !next_fdatasync) _exit(125);
}
static int matches(int fd, int directory, off_t *size) {
    const char *root = getenv("KEI_TEST_LATE_IO_ROOT");
    char control[PATH_MAX], link[64], path[PATH_MAX];
    struct stat metadata;
    if (!root || snprintf(control, sizeof(control), "%s/fault-enabled", root) >= (int)sizeof(control)
        || access(control, F_OK) != 0 || fstat(fd, &metadata) != 0) return 0;
    if (directory ? !S_ISDIR(metadata.st_mode) : !S_ISREG(metadata.st_mode)) return 0;
    if (snprintf(link, sizeof(link), "/proc/self/fd/%d", fd) >= (int)sizeof(link)) return 0;
    ssize_t n = readlink(link, path, sizeof(path)-1);
    if (n < 0) return 0;
    path[n] = 0;
    size_t length = strlen(root);
    if (strncmp(root, path, length) != 0 || (path[length] != '/' && path[length] != 0)) return 0;
    if (!directory && (n < 5 || strcmp(path+n-5, ".part") != 0)) return 0;
    *size = metadata.st_size;
    return 1;
}
static void reached(const char *operation, int failure, off_t progress) {
    const char *root = getenv("KEI_TEST_LATE_IO_ROOT");
    char path[PATH_MAX], message[96];
    if (!root || snprintf(path,sizeof(path),"%s/fault-reached",root) >= (int)sizeof(path)) _exit(126);
    int count=snprintf(message,sizeof(message),"%s errno=%d progress=%lld\n",operation,failure,(long long)progress);
    int audit=open(path,O_WRONLY|O_CREAT|O_TRUNC|O_NOFOLLOW,0600);
    if (audit < 0 || count < 0 || count >= (int)sizeof(message) || next_write(audit,message,(size_t)count)!=count) _exit(127);
    close(audit);
}
ssize_t write(int fd, const void *buffer, size_t length) {
    pthread_once(&init_once, init_calls);
    const char *mode=getenv("KEI_TEST_LATE_IO_CASE");
    off_t size;
    if (mode && (strcmp(mode,"write_enospc")==0 || strcmp(mode,"flush_transport")==0 || strcmp(mode,"flush_cancel")==0)
        && length>0 && matches(fd,0,&size)) {
        ssize_t written=next_write(fd,buffer,length<4?length:4);
        if (written<=0) return written;
        int failure=strcmp(mode,"write_enospc")==0?ENOSPC:EIO;
        reached("write/flush",failure,size+written);
        /* The Rust fixture closes transport or requests cancellation while
         * Tokio still owns this in-flight write. Its flush must settle EIO. */
        if (failure==EIO) usleep(200000);
        errno=failure;
        return -1;
    }
    return next_write(fd,buffer,length);
}
int fdatasync(int fd) {
    pthread_once(&init_once, init_calls);
    const char *mode=getenv("KEI_TEST_LATE_IO_CASE");
    off_t size;
    if (mode && strcmp(mode,"file_sync")==0 && matches(fd,0,&size) && size>0) {
        reached("fdatasync",EIO,size); errno=EIO; return -1;
    }
    return next_fdatasync(fd);
}
int fsync(int fd) {
    pthread_once(&init_once, init_calls);
    const char *mode=getenv("KEI_TEST_LATE_IO_CASE");
    off_t size;
    if (mode && ((strcmp(mode,"directory_sync")==0 && matches(fd,1,&size))
        || ((strcmp(mode,"reconcile_sync")==0 || strcmp(mode,"sidecar_sync")==0) && matches(fd,0,&size) && size>0))) {
        reached("fsync",EIO,size); errno=EIO; return -1;
    }
    return next_fsync(fd);
}
