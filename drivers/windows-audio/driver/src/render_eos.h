#pragma once
// Pure packet-boundary policy, shared with the native regression test.
// ULONG/LONG are Windows 32-bit types supplied by the including translation unit.
typedef struct _QPWGRAPH_RENDER_PAYLOAD {
  ULONG Bytes;
  LONG EosState;
} QPWGRAPH_RENDER_PAYLOAD;

static QPWGRAPH_RENDER_PAYLOAD
QpwgraphRenderPayload(ULONG Packet, ULONG PacketBytes, LONG EosState,
                     ULONG EosPacket, ULONG EosBytes) {
  QPWGRAPH_RENDER_PAYLOAD result = {PacketBytes, EosState};
  if (EosState == 2) {
    result.Bytes = 0;
  } else if (EosState == 1 && (LONG)(Packet - EosPacket) >= 0) {
    result.Bytes = Packet == EosPacket && EosBytes <= PacketBytes ? EosBytes : 0;
    result.EosState = 2;
  }
  return result;
}
