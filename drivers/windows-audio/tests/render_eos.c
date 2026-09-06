#include <stdint.h>
#include <assert.h>
typedef uint32_t ULONG;
typedef int32_t LONG;
#include "../driver/src/render_eos.h"

int main(void) {
  QPWGRAPH_RENDER_PAYLOAD payload;
  // An ordinary packet and the packet preceding EOS keep their full payload.
  assert(QpwgraphRenderPayload(9, 1920, 0, 0, 0).Bytes == 1920);
  assert(QpwgraphRenderPayload(9, 1920, 1, 10, 40).Bytes == 1920);
  // The final packet retains exactly its declared prefix, including empty/full.
  for (ULONG bytes = 0; bytes <= 1920; bytes += 4) {
    payload = QpwgraphRenderPayload(10, 1920, 1, 10, bytes);
    assert(payload.Bytes == bytes && payload.EosState == 2);
    assert(QpwgraphRenderPayload(11, 1920, payload.EosState, 10, bytes).Bytes == 0);
  }
  // A skipped final packet must not allow circular-buffer replay.
  payload = QpwgraphRenderPayload(11, 1920, 1, 10, 40);
  assert(payload.Bytes == 0 && payload.EosState == 2);
  assert(QpwgraphRenderPayload(10, 1920, 1, 10, 1924).Bytes == 0);
  // Sequence rollover does not confuse "before EOS" with "after EOS".
  assert(QpwgraphRenderPayload(UINT32_MAX, 1920, 1, 0, 40).Bytes == 1920);
  payload = QpwgraphRenderPayload(0, 1920, 1, UINT32_MAX, 40);
  assert(payload.Bytes == 0 && payload.EosState == 2);
  return 0;
}
