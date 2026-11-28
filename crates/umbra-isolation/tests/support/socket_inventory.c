#include <errno.h>
#include <libproc.h>
#include <stdint.h>

/* Test hook: use the SDK layout for socket identity and kind. */
int umbra_inventory_socket(int pid, int fd, uint64_t *identity,
                           int *domain, int *type) {
    struct socket_fdinfo info = {0};
    int bytes = proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, &info, sizeof(info));
    if (bytes != (int)sizeof(info)) {
        if (bytes >= 0) errno = EPROTO;
        return -1;
    }
    if (!info.psi.soi_so) { errno = ENODATA; return -1; }
    *identity = info.psi.soi_so;
    *domain = info.psi.soi_family;
    *type = info.psi.soi_type;
    return 0;
}
