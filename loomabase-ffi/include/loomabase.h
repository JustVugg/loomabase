/*
 * C ABI for the Loomabase CRDT protocol core.
 *
 * Link against the `loomabase_ffi` cdylib. All strings are UTF-8. Strings
 * returned by the library are owned by the caller and must be released with
 * loomabase_string_free.
 */
#ifndef LOOMABASE_H
#define LOOMABASE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque in-memory reference server. */
typedef struct LoomabaseState LoomabaseState;

/* Stable ABI version implemented by the loaded library. */
uint32_t loomabase_abi_version(void);

/* Latest error for the calling thread, or NULL. The borrowed pointer remains
 * valid until that thread calls another Loomabase function. */
const char *loomabase_last_error_message(void);

/* Create a reference server for the `todos` contract. Free with
 * loomabase_state_free. */
LoomabaseState *loomabase_state_new(void);

/* Merge a JSON-encoded SyncPayload for `device_id`. Returns a newly allocated
 * JSON response string, or NULL on error. Free the result with
 * loomabase_string_free. */
char *loomabase_state_merge(LoomabaseState *state, const char *payload_json,
                            const char *device_id);

/* Free a reference server. */
void loomabase_state_free(LoomabaseState *state);

/* Free a string returned by this library. */
void loomabase_string_free(char *string);

#ifdef __cplusplus
}
#endif

#endif /* LOOMABASE_H */
