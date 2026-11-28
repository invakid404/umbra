#include <errno.h>
#include <libproc.h>
#include <stdint.h>

/* Qualification only: use the SDK's complete socket_fdinfo layout. */
int umbra_probe_socket_identity(int pid, int fd, uint64_t *identity) {
    struct socket_fdinfo info = {0};
    int bytes = proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, &info, sizeof(info));
    if (bytes != (int)sizeof(info)) {
        if (bytes > 0) errno = EPROTO;
        return -1;
    }
    if (!info.psi.soi_so) {
        errno = ENODATA;
        return -1;
    }
    *identity = info.psi.soi_so;
    return 0;
}
