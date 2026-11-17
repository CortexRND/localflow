#include <libproc.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
  if (argc != 2) return 2;
  char *end;
  long pid = strtol(argv[1], &end, 10);
  if (*end || pid <= 0 || pid > INT32_MAX) return 2;
  struct rusage_info_v4 info = {0};
  if (proc_pid_rusage((int)pid, RUSAGE_INFO_V4, (rusage_info_t *)&info)) {
    perror("proc_pid_rusage");
    return 1;
  }
  printf("{\"rss\":%" PRIu64 ",\"footprint\":%" PRIu64 "}\n",
         (uint64_t)info.ri_resident_size, (uint64_t)info.ri_phys_footprint);
  return 0;
}
