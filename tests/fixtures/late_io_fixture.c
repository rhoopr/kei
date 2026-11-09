/* Linux-only qualification fixture. Loaded into one disposable test child,
 * never into kei. The immutable child environment names an exclusive root;
 * only regular .part files beneath that root and explicitly selected
 * parent-directory fsyncs qualify.
 * All forwarding signatures match libc, buffers are bounded, and pthread_once
 * resolves RTLD_NEXT before concurrent calls. No provider data is recorded. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
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
/* Markers are atomically renamed: seeing one proves all its bytes exist. */
static void marker(const char *name, const char *message) {
    const char *root = getenv("KEI_TEST_LATE_IO_ROOT");
    char path[PATH_MAX], temporary[PATH_MAX];
    if (!root || snprintf(path,sizeof(path),"%s/%s",root,name) >= (int)sizeof(path)
        || snprintf(temporary,sizeof(temporary),"%s/%s.tmp",root,name) >= (int)sizeof(temporary)) _exit(126);
    int audit=open(temporary,O_WRONLY|O_CREAT|O_TRUNC|O_NOFOLLOW,0600);
    size_t count=strlen(message);
    if (audit < 0 || next_write(audit,message,count)!=(ssize_t)count) _exit(127);
    if (close(audit)!=0 || rename(temporary,path)!=0) _exit(127);
}
static void reached(const char *operation, int failure, off_t progress) {
    char message[96];
    int count=snprintf(message,sizeof(message),"%s errno=%d progress=%lld\n",operation,failure,(long long)progress);
    if (count < 0 || count >= (int)sizeof(message)) _exit(127);
    marker("fault-reached",message);
}
static void await_transport_stop(void) {
    const char *root=getenv("KEI_TEST_LATE_IO_ROOT");
    char path[PATH_MAX];
    struct timespec start, now, pause={0,1000000};
    if (!root || snprintf(path,sizeof(path),"%s/fault-release",root) >= (int)sizeof(path)
        || clock_gettime(CLOCK_MONOTONIC,&start)!=0) _exit(128);
    while (access(path,F_OK)!=0) {
        if (clock_gettime(CLOCK_MONOTONIC,&now)!=0 || now.tv_sec-start.tv_sec>=10) _exit(129);
        nanosleep(&pause,NULL);
    }
}
/* Inject the parent-directory fsync failure only after the current exchange
 * publication has installed changed.JPG.xmp. The test supplies the original
 * sidecar dev/ino before enabling the fault, so earlier parent fsyncs and any
 * absent or replaced sidecar are not matched. */
static int installed_sidecar_parent(int fd, off_t *progress) {
    const char *root=getenv("KEI_TEST_LATE_IO_ROOT");
    char link[64], path[PATH_MAX], parent[PATH_MAX], target[PATH_MAX], marker[PATH_MAX];
    struct stat metadata;
    if (!root || snprintf(link,sizeof(link),"/proc/self/fd/%d",fd)>=(int)sizeof(link)) return 0;
    ssize_t n=readlink(link,path,sizeof(path)-1);
    if (n<0) return 0;
    path[n]=0;
    if (snprintf(parent,sizeof(parent),"%s/media/A",root)>=(int)sizeof(parent)
        || strcmp(path,parent)!=0) return 0;
    if (snprintf(target,sizeof(target),"%s/media/A/changed.JPG.xmp",root)>=(int)sizeof(target)
        || stat(target,&metadata)!=0 || !S_ISREG(metadata.st_mode) || metadata.st_size<=0) return 0;
    if (snprintf(marker,sizeof(marker),"%s/sidecar-original-identity",root)>=(int)sizeof(marker)) return 0;
    int control=open(marker,O_RDONLY|O_NOFOLLOW);
    if (control<0) return 0;
    char identity[128];
    ssize_t length=read(control,identity,sizeof(identity)-1);
    close(control);
    if (length<=0) return 0;
    identity[length]=0;
    unsigned long long original_dev=0, original_ino=0;
    if (sscanf(identity,"%llu %llu",&original_dev,&original_ino)!=2) return 0;
    if ((unsigned long long)metadata.st_dev==original_dev
        && (unsigned long long)metadata.st_ino==original_ino) return 0;
    *progress=metadata.st_size;
    return 1;
}
ssize_t write(int fd, const void *buffer, size_t length) {
    pthread_once(&init_once, init_calls);
    const char *mode=getenv("KEI_TEST_LATE_IO_CASE");
    off_t size;
    if (mode && (strcmp(mode,"write_enospc")==0 || strcmp(mode,"write_boundary")==0 || strcmp(mode,"flush_transport")==0 || strcmp(mode,"flush_cancel")==0)
        && length>0 && matches(fd,0,&size)) {
        ssize_t written=next_write(fd,buffer,length<4?length:4);
        if (written<=0) return written;
        int failure=(strcmp(mode,"write_enospc")==0 || strcmp(mode,"write_boundary")==0)?ENOSPC:EIO;
        reached("write/flush",failure,size+written);
        /* The child closes transport or requests cancellation before release.
         * This bounded rendezvous replaces a scheduling-duration assumption. */
        if (failure==EIO) await_transport_stop();
        marker("fault-returning","write error returning\n");
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
    if (mode && strcmp(mode,"sidecar_parent")==0 && installed_sidecar_parent(fd,&size)) {
        const char *root=getenv("KEI_TEST_LATE_IO_ROOT");
        char control[PATH_MAX];
        int fault = root
            && snprintf(control,sizeof(control),"%s/fault-enabled",root)<(int)sizeof(control)
            && access(control,F_OK)==0;
        if (fault) {
            reached("fsync",EIO,size); errno=EIO; return -1;
        }
        int forwarded=next_fsync(fd);
        if (forwarded!=0) return forwarded;
        char message[96];
        int count=snprintf(message,sizeof(message),"fsync progress=%lld\n",(long long)size);
        if (count<0 || count>=(int)sizeof(message)) _exit(127);
        marker("parent-fsync-success",message);
        return 0;
    }
    if (mode && ((strcmp(mode,"directory_sync")==0 && matches(fd,1,&size))
        || (strcmp(mode,"sidecar_sync")==0 && matches(fd,0,&size) && size>0))) {
        reached("fsync",EIO,size); errno=EIO; return -1;
    }
    return next_fsync(fd);
}
