#include <stdint.h>
#include <stdlib.h>
#include <time.h>

/* No file operations during the quiet phase. */
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    unsigned seconds = (unsigned)strtoul(argv[1], NULL, 10);
    if (!seconds || seconds > 60) return 2;
    struct timespec start, now;
    if (clock_gettime(CLOCK_MONOTONIC, &start)) return 3;
    volatile uint64_t value = 1;
    do {
        for (unsigned i = 0; i < 100000; ++i) value = value * 6364136223846793005ULL + 1;
        if (clock_gettime(CLOCK_MONOTONIC, &now)) return 3;
    } while ((double)(now.tv_sec - start.tv_sec) +
             (double)(now.tv_nsec - start.tv_nsec) / 1000000000.0 < seconds);
    return 0;
}
