/* Measurement-only interposition; never linked into shipped binaries. */
#include "onnxruntime_c_api.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <mach-o/dyld.h>
#include <mach-o/nlist.h>
#include <string.h>

static const OrtApi *original;
static OrtApi api;
static OrtApiBase base;
static pthread_once_t once = PTHREAD_ONCE_INIT;

static OrtStatus *options(const OrtSessionOptions *source, OrtSessionOptions **copy) {
  OrtStatus *status = original->CloneSessionOptions(source, copy);
  if (status) return status;
  const char *value;
#define APPLY(key, operation)                                                    \
  if ((value = getenv(key))) {                                                  \
    status = (operation);                                                      \
    if (status) { original->ReleaseSessionOptions(*copy); return status; }      \
  }
  APPLY("LF_ORT_INTRA", original->SetIntraOpNumThreads(*copy, atoi(value)))
  APPLY("LF_ORT_INTER", original->SetInterOpNumThreads(*copy, atoi(value)))
  APPLY("LF_ORT_ARENA", original->DisableCpuMemArena(*copy))
  APPLY("LF_ORT_PATTERN", original->DisableMemPattern(*copy))
  APPLY("LF_ORT_GRAPH", original->SetSessionGraphOptimizationLevel(*copy, (GraphOptimizationLevel)atoi(value)))
#undef APPLY
  fprintf(stderr, "ORT_MEMORY_OPTIONS applied: intra=%s inter=%s arena_off=%s pattern_off=%s graph=%s\n",
          getenv("LF_ORT_INTRA"), getenv("LF_ORT_INTER"), getenv("LF_ORT_ARENA"),
          getenv("LF_ORT_PATTERN"), getenv("LF_ORT_GRAPH"));
  return NULL;
}

static OrtStatus *ORT_API_CALL create(const OrtEnv *env, const char *path,
                                     const OrtSessionOptions *source, OrtSession **session) {
  OrtSessionOptions *copy;
  OrtStatus *status = options(source, &copy);
  if (status) return status;
  status = original->CreateSession(env, path, copy, session);
  original->ReleaseSessionOptions(copy);
  return status;
}

static OrtStatus *ORT_API_CALL create_array(const OrtEnv *env, const void *data, size_t length,
                                           const OrtSessionOptions *source, OrtSession **session) {
  OrtSessionOptions *copy;
  OrtStatus *status = options(source, &copy);
  if (status) return status;
  status = original->CreateSessionFromArray(env, data, length, copy, session);
  original->ReleaseSessionOptions(copy);
  return status;
}

static const OrtApi *ORT_API_CALL get_api(uint32_t version) {
  if (version > ORT_API_VERSION) return NULL;
  return &api;
}

static void initialize(void) {
  const OrtApiBase *(*get_base)(void) = NULL;
  /* Read the original export rather than a dyld-interposed dlsym result. */
  for (uint32_t i = 0; i < _dyld_image_count() && !get_base; ++i) {
    const char *name = _dyld_get_image_name(i);
    if (!name || !strstr(name, "/libonnxruntime")) continue;
    const struct mach_header_64 *header = (const void *)_dyld_get_image_header(i);
    if (header->magic != MH_MAGIC_64) continue;
    intptr_t slide = _dyld_get_image_vmaddr_slide(i);
    const struct symtab_command *symbols = NULL;
    const struct segment_command_64 *linkedit = NULL;
    const struct load_command *command = (const void *)(header + 1);
    for (uint32_t j = 0; j < header->ncmds; ++j) {
      if (command->cmd == LC_SYMTAB) symbols = (const void *)command;
      if (command->cmd == LC_SEGMENT_64) {
        const struct segment_command_64 *segment = (const void *)command;
        if (!strcmp(segment->segname, SEG_LINKEDIT)) linkedit = segment;
      }
      command = (const void *)((const char *)command + command->cmdsize);
    }
    if (!symbols || !linkedit) continue;
    uintptr_t mapped = slide + linkedit->vmaddr - linkedit->fileoff;
    const struct nlist_64 *entries = (const void *)(mapped + symbols->symoff);
    const char *strings = (const void *)(mapped + symbols->stroff);
    for (uint32_t j = 0; j < symbols->nsyms; ++j) {
      if ((entries[j].n_type & N_TYPE) == N_SECT && entries[j].n_un.n_strx < symbols->strsize &&
          !strcmp(strings + entries[j].n_un.n_strx, "_OrtGetApiBase")) {
        get_base = (void *)(slide + entries[j].n_value);
        fprintf(stderr, "ORT_MEMORY_OPTIONS original image: %s\n", name);
        break;
      }
    }
  }
  if (!get_base) { fprintf(stderr, "ORT_MEMORY_OPTIONS original export missing\n"); exit(78); }
  base = *get_base();
  original = base.GetApi(ORT_API_VERSION);
  if (!original) { fprintf(stderr, "ORT_MEMORY_OPTIONS API %u unavailable (runtime %s)\n", ORT_API_VERSION, base.GetVersionString()); exit(78); }
  fprintf(stderr, "ORT_MEMORY_OPTIONS API %u / runtime %s\n", ORT_API_VERSION, base.GetVersionString());
  api = *original;
  api.CreateSession = create;
  api.CreateSessionFromArray = create_array;
  base.GetApi = get_api;
}

static const OrtApiBase *ORT_API_CALL interposed_base(void) {
  pthread_once(&once, initialize);
  return &base;
}

__attribute__((used, section("__DATA,__interpose")))
static const struct { const void *replacement; const void *replacee; } interpose = {
  (const void *)&interposed_base, (const void *)&OrtGetApiBase
};
