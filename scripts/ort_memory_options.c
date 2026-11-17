/* Measurement-only interposition; never linked into shipped binaries. */
#include "onnxruntime_c_api.h"
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>

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
  const OrtApiBase *(*get_base)(void) = dlsym(RTLD_NEXT, "OrtGetApiBase");
  if (!get_base) abort();
  base = *get_base();
  original = base.GetApi(ORT_API_VERSION);
  if (!original) abort();
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
