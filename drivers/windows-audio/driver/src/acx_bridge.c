#include <wdm.h>
#include <windef.h>
#include <ks.h>
#define NOBITMAP
#include <mmreg.h>
#include <ksmedia.h>

#include "acx_wrapper.h"

#define QPWGRAPH_DRIVER_TAG ((ULONG)'aPWQ')
#define QPWGRAPH_MAX_PACKET_COUNT 8
#define QPWGRAPH_HNS_PER_SEC 10000000ULL

static const GUID QpwgraphRenderComponentGuid = {
    0x9f1d8d45,
    0x3f0b,
    0x4b8e,
    {0x8a, 0x92, 0x6e, 0x22, 0x4d, 0x9b, 0xa1, 0x70}};

static const GUID QpwgraphCaptureComponentGuid = {
    0x4c4e51a2,
    0x2e1a,
    0x4c68,
    {0x9f, 0x71, 0x31, 0x7c, 0x8f, 0x4d, 0x2a, 0x06}};

static const GUID QpwgraphRelayRenderComponentGuid = {
    0x2a2a6d04,
    0x5d8c,
    0x49a0,
    {0x9c, 0x65, 0x70, 0x1d, 0x6e, 0x5a, 0x21, 0x83}};

static const GUID QpwgraphRelayCaptureComponentGuid = {
    0x7f6e5b11,
    0x0a46,
    0x4e7a,
    {0x8a, 0x2f, 0x49, 0x3c, 0x2d, 0x84, 0xb7, 0x19}};

typedef enum _QPWGRAPH_CABLE {
  QpwgraphAppCable = 0,
  QpwgraphRelayCable = 1,
} QPWGRAPH_CABLE;

static KSDATAFORMAT_WAVEFORMATEXTENSIBLE QpwgraphPcm48000Stereo = {
    {sizeof(KSDATAFORMAT_WAVEFORMATEXTENSIBLE), 0, 0, 0,
     STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
     STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
     STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)},
    {{WAVE_FORMAT_EXTENSIBLE, 2, 48000, 192000, 4, 16,
      sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)},
     16,
     KSAUDIO_SPEAKER_STEREO,
     STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM)}};

typedef struct _QPWGRAPH_DEVICE_CONTEXT {
  ACXCIRCUIT RenderCircuit;
  ACXCIRCUIT MonitorCircuit;
  ACXCIRCUIT RelayRenderCircuit;
  ACXCIRCUIT RelayCaptureCircuit;
  BOOLEAN RenderCircuitAdded;
  BOOLEAN MonitorCircuitAdded;
  BOOLEAN RelayRenderCircuitAdded;
  BOOLEAN RelayCaptureCircuitAdded;
  volatile LONG ActiveRenderStreams;
  volatile LONG ActiveCaptureStreams;
  volatile LONG ActiveRelayRenderStreams;
  volatile LONG ActiveRelayCaptureStreams;
} QPWGRAPH_DEVICE_CONTEXT, *PQPWGRAPH_DEVICE_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(QPWGRAPH_DEVICE_CONTEXT,
                                   QpwgraphGetDeviceContext)

typedef struct _QPWGRAPH_TIMER_CONTEXT {
  ACXSTREAM Stream;
} QPWGRAPH_TIMER_CONTEXT, *PQPWGRAPH_TIMER_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(QPWGRAPH_TIMER_CONTEXT,
                                   QpwgraphGetTimerContext)

typedef struct _QPWGRAPH_STREAM_CONTEXT {
  ACXSTREAM Stream;
  ACXDATAFORMAT StreamFormat;
  PQPWGRAPH_DEVICE_CONTEXT DeviceContext;
  QPWGRAPH_CABLE Cable;
  BOOLEAN IsCapture;
  BOOLEAN CountedRunning;
  WDFTIMER NotificationTimer;
  ACX_STREAM_STATE State;
  ULONG PacketCount;
  ULONG PacketSize;
  ULONG FirstPacketOffset;
  ULONG BytesPerSecond;
  volatile LONG CurrentPacket;
  volatile LONG64 Position;
  volatile LONG64 StartTime;
  volatile LONG64 StartPosition;
  volatile LONG64 GlitchAdjust;
  volatile LONG64 CurrentPacketStart;
  volatile LONG64 LastPacketStart;
  LARGE_INTEGER PerformanceCounterFrequency;
  PVOID PacketBuffers[QPWGRAPH_MAX_PACKET_COUNT];
} QPWGRAPH_STREAM_CONTEXT, *PQPWGRAPH_STREAM_CONTEXT;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(QPWGRAPH_STREAM_CONTEXT,
                                   QpwgraphGetStreamContext)

extern ULONG qpwgraph_audio_transport_push_pcm16(const UCHAR *Data,
                                                 ULONG Bytes);
extern ULONG qpwgraph_audio_transport_pop_pcm16(UCHAR *Data, ULONG Bytes);
extern VOID qpwgraph_audio_transport_clear(VOID);
extern ULONG qpwgraph_audio_transport_push_relay_pcm16(const UCHAR *Data,
                                                       ULONG Bytes);
extern ULONG qpwgraph_audio_transport_pop_relay_pcm16(UCHAR *Data, ULONG Bytes);
extern VOID qpwgraph_audio_transport_clear_relay(VOID);

static NTSTATUS
QpwgraphEvtDevicePrepareHardware(WDFDEVICE Device, WDFCMRESLIST ResourcesRaw,
                                 WDFCMRESLIST ResourcesTranslated);

static NTSTATUS
QpwgraphEvtDeviceReleaseHardware(WDFDEVICE Device,
                                 WDFCMRESLIST ResourcesTranslated);

static NTSTATUS QpwgraphEvtRenderCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments);

static NTSTATUS QpwgraphEvtCaptureCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments);

static NTSTATUS QpwgraphEvtRelayRenderCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments);

static NTSTATUS QpwgraphEvtRelayCaptureCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments);

static NTSTATUS QpwgraphAddBridgeJack(ACXPIN Pin) {
  ACX_JACK_CONFIG jackConfig;
  ACXJACK jack;
  WDF_OBJECT_ATTRIBUTES attributes;
  NTSTATUS status;

  if (Pin == NULL) {
    return STATUS_INVALID_PARAMETER;
  }

  ACX_JACK_CONFIG_INIT(&jackConfig);
  jackConfig.Description.ChannelMapping =
      SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
  jackConfig.Description.Color = 0;
  jackConfig.Description.ConnectionType = AcxConnTypeAtapiInternal;
  jackConfig.Description.GeoLocation = AcxGeoLocFront;
  jackConfig.Description.GenLocation = AcxGenLocPrimaryBox;
  jackConfig.Description.PortConnection = AcxPortConnIntegratedDevice;

  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = Pin;
  status = AcxJackCreate(Pin, &attributes, &jackConfig, &jack);
  if (!NT_SUCCESS(status)) {
    return status;
  }
  return AcxPinAddJacks(Pin, &jack, 1);
}

static VOID QpwgraphEvtStreamDestroy(WDFOBJECT Object);

static NTSTATUS QpwgraphEvtStreamGetHwLatency(ACXSTREAM Stream, PULONG FifoSize,
                                              PULONG Delay);

static NTSTATUS QpwgraphEvtStreamAllocateRtPackets(ACXSTREAM Stream,
                                                   ULONG PacketCount,
                                                   ULONG PacketSize,
                                                   PACX_RTPACKET *Packets);

static VOID QpwgraphEvtStreamFreeRtPackets(ACXSTREAM Stream,
                                           PACX_RTPACKET Packets,
                                           ULONG PacketCount);

static NTSTATUS QpwgraphEvtStreamPrepareHardware(ACXSTREAM Stream);
static NTSTATUS QpwgraphEvtStreamReleaseHardware(ACXSTREAM Stream);
static NTSTATUS QpwgraphEvtStreamRun(ACXSTREAM Stream);
static NTSTATUS QpwgraphEvtStreamPause(ACXSTREAM Stream);

static NTSTATUS QpwgraphEvtStreamSetRenderPacket(ACXSTREAM Stream, ULONG Packet,
                                                 ULONG Flags,
                                                 ULONG EosPacketLength);

static NTSTATUS QpwgraphEvtStreamGetCapturePacket(ACXSTREAM Stream,
                                                  PULONG LastCapturePacket,
                                                  PULONGLONG QpcPacketStart,
                                                  PBOOLEAN MoreData);

static NTSTATUS QpwgraphEvtStreamGetCurrentPacket(ACXSTREAM Stream,
                                                  PULONG CurrentPacket);

static NTSTATUS QpwgraphEvtStreamGetPresentationPosition(
    ACXSTREAM Stream, PULONGLONG PositionInBlocks, PULONGLONG QpcPosition);

static VOID QpwgraphEvtTimerPass(WDFTIMER Timer);

static ULONGLONG QpwgraphNowHns(PQPWGRAPH_STREAM_CONTEXT Context) {
  LARGE_INTEGER counter;

  counter = KeQueryPerformanceCounter(NULL);
  return (ULONGLONG)KSCONVERT_PERFORMANCE_TIME(
      Context->PerformanceCounterFrequency.QuadPart, counter);
}

static VOID QpwgraphUpdatePosition(PQPWGRAPH_STREAM_CONTEXT Context) {
  ULONGLONG now;
  ULONGLONG start;
  ULONGLONG elapsed;
  ULONGLONG glitch;
  ULONGLONG startPosition;
  ULONGLONG position;
  LONGLONG previous;

  if (Context->State != AcxStreamStateRun || Context->BytesPerSecond == 0) {
    return;
  }

  now = QpwgraphNowHns(Context);
  start = (ULONGLONG)InterlockedCompareExchange64(&Context->StartTime, -1, -1);
  if (now < start) {
    return;
  }

  elapsed = now - start;
  glitch =
      (ULONGLONG)InterlockedCompareExchange64(&Context->GlitchAdjust, -1, -1);
  startPosition =
      (ULONGLONG)InterlockedCompareExchange64(&Context->StartPosition, -1, -1);
  position = startPosition;
  if (elapsed >= glitch) {
    position +=
        (elapsed - glitch) * Context->BytesPerSecond / QPWGRAPH_HNS_PER_SEC;
  }

  previous = InterlockedCompareExchange64(&Context->Position, -1, -1);
  if ((ULONGLONG)previous > position) {
    position = (ULONGLONG)previous;
  }
  InterlockedExchange64(&Context->Position, (LONG64)position);
}

static VOID QpwgraphScheduleNextPass(PQPWGRAPH_STREAM_CONTEXT Context);

static PVOID QpwgraphPacketBuffer(PQPWGRAPH_STREAM_CONTEXT Context,
                                  ULONG Packet) {
  ULONG index;
  PUCHAR buffer;

  if (Context == NULL || Context->PacketCount == 0 ||
      Context->PacketCount > QPWGRAPH_MAX_PACKET_COUNT) {
    return NULL;
  }
  index = Packet % Context->PacketCount;
  buffer = (PUCHAR)Context->PacketBuffers[index];
  if (buffer == NULL) {
    return NULL;
  }
  if (index == 0) {
    buffer += Context->FirstPacketOffset;
  }
  return buffer;
}

static BOOLEAN
QpwgraphCableHasActiveStreams(PQPWGRAPH_DEVICE_CONTEXT DeviceContext,
                              QPWGRAPH_CABLE Cable) {
  if (DeviceContext == NULL) {
    return FALSE;
  }
  if (Cable == QpwgraphRelayCable) {
    return InterlockedCompareExchange(&DeviceContext->ActiveRelayRenderStreams,
                                      0, 0) != 0 ||
           InterlockedCompareExchange(&DeviceContext->ActiveRelayCaptureStreams,
                                      0, 0) != 0;
  }
  return InterlockedCompareExchange(&DeviceContext->ActiveRenderStreams, 0,
                                    0) != 0 ||
         InterlockedCompareExchange(&DeviceContext->ActiveCaptureStreams, 0,
         0) != 0;
}

static volatile LONG *
QpwgraphActiveStreamCounter(PQPWGRAPH_STREAM_CONTEXT Context) {
  if (Context == NULL || Context->DeviceContext == NULL) {
    return NULL;
  }
  if (Context->Cable == QpwgraphRelayCable) {
    return Context->IsCapture
               ? &Context->DeviceContext->ActiveRelayCaptureStreams
               : &Context->DeviceContext->ActiveRelayRenderStreams;
  }
  return Context->IsCapture ? &Context->DeviceContext->ActiveCaptureStreams
                            : &Context->DeviceContext->ActiveRenderStreams;
}

static VOID QpwgraphClearCable(QPWGRAPH_CABLE Cable) {
  if (Cable == QpwgraphRelayCable) {
    qpwgraph_audio_transport_clear_relay();
  } else {
    qpwgraph_audio_transport_clear();
  }
}

static VOID QpwgraphStopCountedStream(PQPWGRAPH_STREAM_CONTEXT Context) {
  LONG remainingRender;
  LONG remainingCapture;

  if (Context == NULL || !Context->CountedRunning ||
      Context->DeviceContext == NULL) {
    return;
  }
  Context->CountedRunning = FALSE;
  if (Context->Cable == QpwgraphRelayCable) {
    if (Context->IsCapture) {
      remainingCapture = InterlockedDecrement(
          &Context->DeviceContext->ActiveRelayCaptureStreams);
      remainingRender = InterlockedCompareExchange(
          &Context->DeviceContext->ActiveRelayRenderStreams, 0, 0);
    } else {
      remainingRender = InterlockedDecrement(
          &Context->DeviceContext->ActiveRelayRenderStreams);
      remainingCapture = InterlockedCompareExchange(
          &Context->DeviceContext->ActiveRelayCaptureStreams, 0, 0);
    }
  } else {
    if (Context->IsCapture) {
      remainingCapture =
          InterlockedDecrement(&Context->DeviceContext->ActiveCaptureStreams);
      remainingRender = InterlockedCompareExchange(
          &Context->DeviceContext->ActiveRenderStreams, 0, 0);
    } else {
      remainingRender =
          InterlockedDecrement(&Context->DeviceContext->ActiveRenderStreams);
      remainingCapture = InterlockedCompareExchange(
          &Context->DeviceContext->ActiveCaptureStreams, 0, 0);
    }
  }
  if (remainingRender == 0 && remainingCapture == 0) {
    QpwgraphClearCable(Context->Cable);
  }
}

static VOID QpwgraphStreamPass(PQPWGRAPH_STREAM_CONTEXT Context) {
  LONG activePacket;
  LONG completedPacket;
  PVOID packetBuffer;
  ULONGLONG qpcCompleted;

  if (Context->State != AcxStreamStateRun) {
    return;
  }

  activePacket = InterlockedCompareExchange(&Context->CurrentPacket, -1, -1);
  packetBuffer = QpwgraphPacketBuffer(Context, (ULONG)activePacket);
  if (packetBuffer != NULL && Context->PacketSize != 0) {
    if (Context->IsCapture) {
      if (Context->Cable == QpwgraphRelayCable) {
        (VOID) qpwgraph_audio_transport_pop_relay_pcm16((UCHAR *)packetBuffer,
                                                        Context->PacketSize);
      } else {
        (VOID) qpwgraph_audio_transport_pop_pcm16((UCHAR *)packetBuffer,
                                                  Context->PacketSize);
      }
    } else {
      if (Context->Cable == QpwgraphRelayCable) {
        (VOID) qpwgraph_audio_transport_push_relay_pcm16(
            (const UCHAR *)packetBuffer, Context->PacketSize);
      } else {
        (VOID) qpwgraph_audio_transport_push_pcm16((const UCHAR *)packetBuffer,
                                                   Context->PacketSize);
      }
    }
  }

  completedPacket = InterlockedIncrement(&Context->CurrentPacket) - 1;
  qpcCompleted = (ULONGLONG)KeQueryPerformanceCounter(NULL).QuadPart;
  InterlockedExchange64(&Context->LastPacketStart, Context->CurrentPacketStart);
  InterlockedExchange64(&Context->CurrentPacketStart, (LONG64)qpcCompleted);

  // Render packets enter the Rust-owned bounded cable; capture packets are
  // filled from that same cable with explicit silence on underflow.
  (VOID) AcxRtStreamNotifyPacketComplete(Context->Stream,
                                         (ULONG)completedPacket, qpcCompleted);

  QpwgraphScheduleNextPass(Context);
}

static VOID QpwgraphScheduleNextPass(PQPWGRAPH_STREAM_CONTEXT Context) {
  ULONGLONG currentPacket;
  ULONGLONG nextPacket;
  ULONGLONG nextPacketPosition;
  ULONGLONG startPosition;
  ULONGLONG positionFromPause;
  ULONGLONG packetTime;
  ULONGLONG startTime;
  ULONGLONG glitchAdjust;
  ULONGLONG nextTime;
  ULONGLONG currentTime;
  LONGLONG delay;

  if (Context->State != AcxStreamStateRun ||
      Context->NotificationTimer == NULL || Context->PacketSize == 0 ||
      Context->BytesPerSecond == 0) {
    return;
  }

  currentPacket =
      (ULONG)InterlockedCompareExchange(&Context->CurrentPacket, -1, -1);
  nextPacket = currentPacket + 1;
  nextPacketPosition = nextPacket * (ULONGLONG)Context->PacketSize;
  startPosition =
      (ULONGLONG)InterlockedCompareExchange64(&Context->StartPosition, -1, -1);
  positionFromPause = nextPacketPosition >= startPosition
                          ? nextPacketPosition - startPosition
                          : 0;
  packetTime =
      positionFromPause * QPWGRAPH_HNS_PER_SEC / Context->BytesPerSecond;
  startTime =
      (ULONGLONG)InterlockedCompareExchange64(&Context->StartTime, -1, -1);
  glitchAdjust =
      (ULONGLONG)InterlockedCompareExchange64(&Context->GlitchAdjust, -1, -1);
  nextTime = startTime + glitchAdjust + packetTime;
  currentTime = QpwgraphNowHns(Context);

  if (nextTime <= currentTime) {
    InterlockedExchange64(&Context->GlitchAdjust,
                          (LONG64)(glitchAdjust + currentTime - nextTime));
    QpwgraphStreamPass(Context);
    return;
  }

  delay = -(LONGLONG)(nextTime - currentTime);
  (VOID) WdfTimerStart(Context->NotificationTimer, delay);
}

static VOID QpwgraphEvtTimerPass(WDFTIMER Timer) {
  PQPWGRAPH_TIMER_CONTEXT TimerContext;
  PQPWGRAPH_STREAM_CONTEXT StreamContext;

  TimerContext = QpwgraphGetTimerContext(Timer);
  if (TimerContext == NULL || TimerContext->Stream == NULL) {
    return;
  }

  StreamContext = QpwgraphGetStreamContext(TimerContext->Stream);
  if (StreamContext == NULL) {
    return;
  }

  QpwgraphStreamPass(StreamContext);
}

static VOID QpwgraphFreeRtPacketArray(PACX_RTPACKET Packets, ULONG PacketCount,
                                      PQPWGRAPH_STREAM_CONTEXT Context) {
  ULONG index;

  if (Packets == NULL) {
    return;
  }

  if (PacketCount > QPWGRAPH_MAX_PACKET_COUNT) {
    PacketCount = QPWGRAPH_MAX_PACKET_COUNT;
  }

  for (index = 0; index < PacketCount; ++index) {
    PMDL mdl = Packets[index].RtPacketBuffer.u.MdlType.Mdl;
    if (mdl != NULL) {
      IoFreeMdl(mdl);
    }
    if (Context != NULL && Context->PacketBuffers[index] != NULL) {
      ExFreePool(Context->PacketBuffers[index]);
      Context->PacketBuffers[index] = NULL;
    }
  }

  ExFreePool(Packets);
}

static NTSTATUS QpwgraphEvtStreamAllocateRtPackets(ACXSTREAM Stream,
                                                   ULONG PacketCount,
                                                   ULONG PacketSize,
                                                   PACX_RTPACKET *Packets) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  PACX_RTPACKET packetArray;
  ULONG packetAllocationSize;
  ULONG firstPacketOffset;
  ULONG index;

  PAGED_CODE();

  if (Packets == NULL || PacketCount == 0 ||
      PacketCount > QPWGRAPH_MAX_PACKET_COUNT || PacketSize == 0 ||
      PacketSize > MAXULONG - (PAGE_SIZE - 1)) {
    return STATUS_INVALID_PARAMETER;
  }

  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }

  packetAllocationSize = (PacketSize + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1);
  firstPacketOffset = packetAllocationSize - PacketSize;
  packetArray = (PACX_RTPACKET)ExAllocatePool2(
      POOL_FLAG_NON_PAGED, sizeof(ACX_RTPACKET) * PacketCount,
      QPWGRAPH_DRIVER_TAG);
  if (packetArray == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  RtlZeroMemory(packetArray, sizeof(ACX_RTPACKET) * PacketCount);
  RtlZeroMemory(Context->PacketBuffers, sizeof(Context->PacketBuffers));

  for (index = 0; index < PacketCount; ++index) {
    PVOID buffer;
    PMDL mdl;

    ACX_RTPACKET_INIT(&packetArray[index]);
    buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, packetAllocationSize,
                             QPWGRAPH_DRIVER_TAG);
    if (buffer == NULL) {
      QpwgraphFreeRtPacketArray(packetArray, index, Context);
      return STATUS_INSUFFICIENT_RESOURCES;
    }

    mdl = IoAllocateMdl(buffer, packetAllocationSize, FALSE, TRUE, NULL);
    if (mdl == NULL) {
      ExFreePool(buffer);
      QpwgraphFreeRtPacketArray(packetArray, index, Context);
      return STATUS_INSUFFICIENT_RESOURCES;
    }

    MmBuildMdlForNonPagedPool(mdl);
    WDF_MEMORY_DESCRIPTOR_INIT_MDL(&packetArray[index].RtPacketBuffer, mdl,
                                   packetAllocationSize);
    packetArray[index].RtPacketSize = PacketSize;
    packetArray[index].RtPacketOffset = index == 0 ? firstPacketOffset : 0;
    Context->PacketBuffers[index] = buffer;
  }

  Context->PacketCount = PacketCount;
  Context->PacketSize = PacketSize;
  Context->FirstPacketOffset = firstPacketOffset;
  Context->BytesPerSecond =
      AcxDataFormatGetAverageBytesPerSec(Context->StreamFormat);
  *Packets = packetArray;
  return STATUS_SUCCESS;
}

static VOID QpwgraphEvtStreamFreeRtPackets(ACXSTREAM Stream,
                                           PACX_RTPACKET Packets,
                                           ULONG PacketCount) {
  PQPWGRAPH_STREAM_CONTEXT Context;

  PAGED_CODE();
  Context = QpwgraphGetStreamContext(Stream);
  QpwgraphFreeRtPacketArray(Packets, PacketCount, Context);
  if (Context != NULL) {
    Context->PacketCount = 0;
    Context->PacketSize = 0;
    Context->FirstPacketOffset = 0;
    Context->BytesPerSecond = 0;
  }
}

static NTSTATUS QpwgraphEvtStreamGetHwLatency(ACXSTREAM Stream, PULONG FifoSize,
                                              PULONG Delay) {
  UNREFERENCED_PARAMETER(Stream);
  PAGED_CODE();
  if (FifoSize == NULL || Delay == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  *FifoSize = 128;
  *Delay = 0;
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamPrepareHardware(ACXSTREAM Stream) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  WDF_TIMER_CONFIG timerConfig;
  WDF_OBJECT_ATTRIBUTES timerAttributes;
  PQPWGRAPH_TIMER_CONTEXT timerContext;
  NTSTATUS status;

  PAGED_CODE();
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  if (Context->State == AcxStreamStatePause) {
    return STATUS_SUCCESS;
  }
  if (Context->State != AcxStreamStateStop) {
    return STATUS_INVALID_DEVICE_STATE;
  }

  if (Context->NotificationTimer == NULL) {
    WDF_TIMER_CONFIG_INIT(&timerConfig, QpwgraphEvtTimerPass);
    timerConfig.AutomaticSerialization = TRUE;
    timerConfig.UseHighResolutionTimer = WdfTrue;
    timerConfig.Period = 0;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&timerAttributes,
                                            QPWGRAPH_TIMER_CONTEXT);
    timerAttributes.ParentObject = Stream;
    status = WdfTimerCreate(&timerConfig, &timerAttributes,
                            &Context->NotificationTimer);
    if (!NT_SUCCESS(status)) {
      return status;
    }
    timerContext = QpwgraphGetTimerContext(Context->NotificationTimer);
    timerContext->Stream = Stream;
  }

  Context->State = AcxStreamStatePause;
  Context->CountedRunning = FALSE;
  InterlockedExchange(&Context->CurrentPacket, 0);
  InterlockedExchange64(&Context->Position, 0);
  InterlockedExchange64(&Context->StartPosition, 0);
  InterlockedExchange64(&Context->GlitchAdjust, 0);
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamReleaseHardware(ACXSTREAM Stream) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  WDFTIMER notificationTimer;

  PAGED_CODE();
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  notificationTimer = Context->NotificationTimer;
  Context->NotificationTimer = NULL;
  if (notificationTimer != NULL) {
    (VOID) WdfTimerStop(notificationTimer, TRUE);
    WdfObjectDelete(notificationTimer);
  }
  QpwgraphStopCountedStream(Context);
  KeFlushQueuedDpcs();
  Context->State = AcxStreamStateStop;
  InterlockedExchange(&Context->CurrentPacket, 0);
  InterlockedExchange64(&Context->Position, 0);
  InterlockedExchange64(&Context->StartPosition, 0);
  InterlockedExchange64(&Context->GlitchAdjust, 0);
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamRun(ACXSTREAM Stream) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  LARGE_INTEGER counter;
  LONGLONG position;

  PAGED_CODE();
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  if (Context->DeviceContext == NULL) {
    return STATUS_INVALID_DEVICE_STATE;
  }
  if (Context->State == AcxStreamStateRun) {
    return STATUS_SUCCESS;
  }
  if (Context->State != AcxStreamStatePause ||
      Context->NotificationTimer == NULL || Context->PacketSize == 0 ||
      Context->BytesPerSecond == 0) {
    return STATUS_INVALID_DEVICE_STATE;
  }

  counter = KeQueryPerformanceCounter(NULL);
  InterlockedExchange64(
      &Context->StartTime,
      (LONG64)KSCONVERT_PERFORMANCE_TIME(
          Context->PerformanceCounterFrequency.QuadPart, counter));
  position = InterlockedCompareExchange64(&Context->Position, -1, -1);
  InterlockedExchange64(&Context->StartPosition, position);
  InterlockedExchange64(&Context->GlitchAdjust, 0);
  if (!Context->CountedRunning) {
    if (!QpwgraphCableHasActiveStreams(Context->DeviceContext,
                                       Context->Cable)) {
      QpwgraphClearCable(Context->Cable);
    }
    {
      volatile LONG *activeCounter = QpwgraphActiveStreamCounter(Context);
      if (activeCounter == NULL ||
          InterlockedCompareExchange(activeCounter, 1, 0) != 0) {
        // The Rust transport is SPSC. Shared-mode audio-engine mixing should
        // present one render stream and one capture stream per cable; never
        // turn a second client into an uncontrolled multi-producer or
        // multi-consumer ring.
        return STATUS_DEVICE_BUSY;
      }
      Context->CountedRunning = TRUE;
    }
  }
  Context->State = AcxStreamStateRun;
  QpwgraphScheduleNextPass(Context);
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamPause(ACXSTREAM Stream) {
  PQPWGRAPH_STREAM_CONTEXT Context;

  PAGED_CODE();
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  if (Context->State == AcxStreamStatePause) {
    return STATUS_SUCCESS;
  }
  if (Context->State != AcxStreamStateRun) {
    return STATUS_INVALID_DEVICE_STATE;
  }

  QpwgraphUpdatePosition(Context);
  if (Context->NotificationTimer != NULL) {
    (VOID) WdfTimerStop(Context->NotificationTimer, TRUE);
  }
  QpwgraphStopCountedStream(Context);
  Context->State = AcxStreamStatePause;
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamSetRenderPacket(ACXSTREAM Stream, ULONG Packet,
                                                 ULONG Flags,
                                                 ULONG EosPacketLength) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  ULONG currentPacket;

  UNREFERENCED_PARAMETER(Flags);
  UNREFERENCED_PARAMETER(EosPacketLength);
  PAGED_CODE();
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }

  currentPacket =
      (ULONG)InterlockedCompareExchange(&Context->CurrentPacket, -1, -1);
  if (Packet <= currentPacket) {
    return STATUS_DATA_LATE_ERROR;
  }
  if (Packet > currentPacket + 1) {
    return STATUS_DATA_OVERRUN;
  }
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamGetCurrentPacket(ACXSTREAM Stream,
                                                  PULONG CurrentPacket) {
  PQPWGRAPH_STREAM_CONTEXT Context;

  PAGED_CODE();
  if (CurrentPacket == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  *CurrentPacket =
      (ULONG)InterlockedCompareExchange(&Context->CurrentPacket, -1, -1);
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtStreamGetPresentationPosition(
    ACXSTREAM Stream, PULONGLONG PositionInBlocks, PULONGLONG QpcPosition) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  ULONG blockAlign;
  LARGE_INTEGER qpc;
  ULONGLONG position;

  PAGED_CODE();
  if (PositionInBlocks == NULL || QpcPosition == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  blockAlign = AcxDataFormatGetBlockAlign(Context->StreamFormat);
  if (blockAlign == 0) {
    return STATUS_INVALID_DEVICE_STATE;
  }
  QpwgraphUpdatePosition(Context);
  position =
      (ULONGLONG)InterlockedCompareExchange64(&Context->Position, -1, -1);
  qpc = KeQueryPerformanceCounter(NULL);
  *PositionInBlocks = position / blockAlign;
  *QpcPosition = (ULONGLONG)qpc.QuadPart;
  return STATUS_SUCCESS;
}

static VOID QpwgraphEvtStreamDestroy(WDFOBJECT Object) {
  PQPWGRAPH_STREAM_CONTEXT Context;

  Context = QpwgraphGetStreamContext((ACXSTREAM)Object);
  if (Context != NULL) {
    if (Context->NotificationTimer != NULL) {
      (VOID) WdfTimerStop(Context->NotificationTimer, TRUE);
      Context->NotificationTimer = NULL;
    }
    QpwgraphStopCountedStream(Context);
  }
}

static NTSTATUS QpwgraphEvtStreamGetCapturePacket(ACXSTREAM Stream,
                                                  PULONG LastCapturePacket,
                                                  PULONGLONG QpcPacketStart,
                                                  PBOOLEAN MoreData) {
  PQPWGRAPH_STREAM_CONTEXT Context;
  ULONG currentPacket;

  PAGED_CODE();
  if (LastCapturePacket == NULL || QpcPacketStart == NULL || MoreData == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  Context = QpwgraphGetStreamContext(Stream);
  if (Context == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  currentPacket =
      (ULONG)InterlockedCompareExchange(&Context->CurrentPacket, -1, -1);
  *LastCapturePacket = currentPacket - 1;
  *QpcPacketStart = (ULONGLONG)InterlockedCompareExchange64(
      &Context->LastPacketStart, -1, -1);
  *MoreData = FALSE;
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphCreateStream(WDFDEVICE Device, ACXCIRCUIT Circuit,
                                     ACXPIN Pin, PACXSTREAM_INIT StreamInit,
                                     ACXDATAFORMAT StreamFormat,
                                     const GUID *SignalProcessingMode,
                                     ACXOBJECTBAG VarArguments,
                                     BOOLEAN IsCapture, QPWGRAPH_CABLE Cable) {
  ACX_STREAM_CALLBACKS streamCallbacks;
  ACX_RT_STREAM_CALLBACKS rtCallbacks;
  WDF_OBJECT_ATTRIBUTES attributes;
  ACXSTREAM stream;
  PQPWGRAPH_DEVICE_CONTEXT DeviceContext;
  PQPWGRAPH_STREAM_CONTEXT Context;
  NTSTATUS status;

  UNREFERENCED_PARAMETER(Pin);
  UNREFERENCED_PARAMETER(SignalProcessingMode);
  UNREFERENCED_PARAMETER(VarArguments);
  PAGED_CODE();

  DeviceContext = QpwgraphGetDeviceContext(Device);
  if (DeviceContext == NULL) {
    return STATUS_INVALID_DEVICE_STATE;
  }

  ACX_STREAM_CALLBACKS_INIT(&streamCallbacks);
  streamCallbacks.EvtAcxStreamPrepareHardware =
      QpwgraphEvtStreamPrepareHardware;
  streamCallbacks.EvtAcxStreamReleaseHardware =
      QpwgraphEvtStreamReleaseHardware;
  streamCallbacks.EvtAcxStreamRun = QpwgraphEvtStreamRun;
  streamCallbacks.EvtAcxStreamPause = QpwgraphEvtStreamPause;
  status = AcxStreamInitAssignAcxStreamCallbacks(StreamInit, &streamCallbacks);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  ACX_RT_STREAM_CALLBACKS_INIT(&rtCallbacks);
  rtCallbacks.EvtAcxStreamGetHwLatency = QpwgraphEvtStreamGetHwLatency;
  rtCallbacks.EvtAcxStreamAllocateRtPackets =
      QpwgraphEvtStreamAllocateRtPackets;
  rtCallbacks.EvtAcxStreamFreeRtPackets = QpwgraphEvtStreamFreeRtPackets;
  if (IsCapture) {
    rtCallbacks.EvtAcxStreamGetCapturePacket =
        QpwgraphEvtStreamGetCapturePacket;
  } else {
    rtCallbacks.EvtAcxStreamSetRenderPacket = QpwgraphEvtStreamSetRenderPacket;
  }
  rtCallbacks.EvtAcxStreamGetCurrentPacket = QpwgraphEvtStreamGetCurrentPacket;
  rtCallbacks.EvtAcxStreamGetPresentationPosition =
      QpwgraphEvtStreamGetPresentationPosition;
  status = AcxStreamInitAssignAcxRtStreamCallbacks(StreamInit, &rtCallbacks);
  if (!NT_SUCCESS(status)) {
    return status;
  }
  AcxStreamInitSetAcxRtStreamSupportsNotifications(StreamInit);

  WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, QPWGRAPH_STREAM_CONTEXT);
  attributes.EvtDestroyCallback = QpwgraphEvtStreamDestroy;
  status =
      AcxRtStreamCreate(Device, Circuit, &attributes, &StreamInit, &stream);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  Context = QpwgraphGetStreamContext(stream);
  Context->Stream = stream;
  Context->StreamFormat = StreamFormat;
  Context->DeviceContext = DeviceContext;
  Context->Cable = Cable;
  Context->IsCapture = IsCapture;
  Context->CountedRunning = FALSE;
  Context->State = AcxStreamStateStop;
  KeQueryPerformanceCounter(&Context->PerformanceCounterFrequency);
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphEvtRenderCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments) {
  return QpwgraphCreateStream(Device, Circuit, Pin, StreamInit, StreamFormat,
                              SignalProcessingMode, VarArguments, FALSE,
                              QpwgraphAppCable);
}

static NTSTATUS QpwgraphEvtCaptureCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments) {
  return QpwgraphCreateStream(Device, Circuit, Pin, StreamInit, StreamFormat,
                              SignalProcessingMode, VarArguments, TRUE,
                              QpwgraphAppCable);
}

static NTSTATUS QpwgraphEvtRelayRenderCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments) {
  return QpwgraphCreateStream(Device, Circuit, Pin, StreamInit, StreamFormat,
                              SignalProcessingMode, VarArguments, FALSE,
                              QpwgraphRelayCable);
}

static NTSTATUS QpwgraphEvtRelayCaptureCircuitCreateStream(
    WDFDEVICE Device, ACXCIRCUIT Circuit, ACXPIN Pin,
    PACXSTREAM_INIT StreamInit, ACXDATAFORMAT StreamFormat,
    const GUID *SignalProcessingMode, ACXOBJECTBAG VarArguments) {
  return QpwgraphCreateStream(Device, Circuit, Pin, StreamInit, StreamFormat,
                              SignalProcessingMode, VarArguments, TRUE,
                              QpwgraphRelayCable);
}

static NTSTATUS QpwgraphCreateCircuit(WDFDEVICE Device, BOOLEAN IsCapture,
                                      QPWGRAPH_CABLE Cable,
                                      ACXCIRCUIT *Circuit) {
  PACXCIRCUIT_INIT circuitInit;
  ACXCIRCUIT circuit;
  ACXPIN pins[2] = {0};
  ACX_PIN_CONFIG pinConfig;
  ACXDATAFORMAT format;
  ACXDATAFORMATLIST formatList;
  ACX_DATAFORMAT_CONFIG formatConfig;
  WDF_OBJECT_ATTRIBUTES attributes;
  UNICODE_STRING circuitName;
  const GUID *componentId;
  NTSTATUS status;

  PAGED_CODE();
  if (Circuit == NULL) {
    return STATUS_INVALID_PARAMETER;
  }
  *Circuit = NULL;

  componentId =
      IsCapture
          ? (Cable == QpwgraphRelayCable ? &QpwgraphRelayCaptureComponentGuid
                                         : &QpwgraphCaptureComponentGuid)
          : (Cable == QpwgraphRelayCable ? &QpwgraphRelayRenderComponentGuid
                                         : &QpwgraphRenderComponentGuid);

  circuitInit = AcxCircuitInitAllocate(Device);
  if (circuitInit == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }

  AcxCircuitInitSetComponentId(circuitInit, componentId);
  if (Cable == QpwgraphRelayCable) {
    if (IsCapture) {
      RtlInitUnicodeString(&circuitName, L"QPWGraphRelayMicrophone");
    } else {
      RtlInitUnicodeString(&circuitName, L"QPWGraphRelaySink");
    }
  } else if (IsCapture) {
    // The circuit name is the symbolic name used by the INF interface
    // section; FriendlyName there remains the user-visible label.
    RtlInitUnicodeString(&circuitName, L"QPWGraphVirtualMonitor");
  } else {
    RtlInitUnicodeString(&circuitName, L"QPWGraphVirtualOutput");
  }
  (VOID) AcxCircuitInitAssignName(circuitInit, &circuitName);
  AcxCircuitInitSetCircuitType(circuitInit, IsCapture ? AcxCircuitTypeCapture
                                                      : AcxCircuitTypeRender);
  status = AcxCircuitInitAssignAcxCreateStreamCallback(
      circuitInit, IsCapture ? (Cable == QpwgraphRelayCable
                                    ? QpwgraphEvtRelayCaptureCircuitCreateStream
                                    : QpwgraphEvtCaptureCircuitCreateStream)
                             : (Cable == QpwgraphRelayCable
                                    ? QpwgraphEvtRelayRenderCircuitCreateStream
                                    : QpwgraphEvtRenderCircuitCreateStream));
  if (!NT_SUCCESS(status)) {
    AcxCircuitInitFree(circuitInit);
    return status;
  }

  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  status = AcxCircuitCreate(Device, &attributes, &circuitInit, &circuit);
  if (!NT_SUCCESS(status)) {
    if (circuitInit != NULL) {
      AcxCircuitInitFree(circuitInit);
    }
    return status;
  }

  ACX_PIN_CONFIG_INIT(&pinConfig);
  pinConfig.Type = IsCapture ? AcxPinTypeSource : AcxPinTypeSink;
  pinConfig.Communication = AcxPinCommunicationSink;
  pinConfig.Category = &KSCATEGORY_AUDIO;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = circuit;
  status = AcxPinCreate(circuit, &attributes, &pinConfig, &pins[0]);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  ACX_PIN_CONFIG_INIT(&pinConfig);
  pinConfig.Type = IsCapture ? AcxPinTypeSink : AcxPinTypeSource;
  pinConfig.Communication = AcxPinCommunicationNone;
  pinConfig.Category = IsCapture ? &KSNODETYPE_MICROPHONE : &KSNODETYPE_SPEAKER;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = circuit;
  status = AcxPinCreate(circuit, &attributes, &pinConfig, &pins[1]);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  // The device-side pin owns the jack descriptor.  ACX's standalone render
  // and capture samples publish the endpoint topology this way; a category
  // alone is not enough to describe the physical side of the bridge pin.
  status = QpwgraphAddBridgeJack(pins[1]);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  ACX_DATAFORMAT_CONFIG_INIT_KS(&formatConfig, &QpwgraphPcm48000Stereo);
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = circuit;
  status = AcxDataFormatCreate(Device, &attributes, &formatConfig, &format);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  formatList = AcxPinGetRawDataFormatList(pins[0]);
  if (formatList == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  status = AcxDataFormatListAddDataFormat(formatList, format);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  status = AcxCircuitAddPins(circuit, pins, 2);
  if (!NT_SUCCESS(status)) {
    return status;
  }
  *Circuit = circuit;
  return STATUS_SUCCESS;
}

static NTSTATUS QpwgraphCreateRenderCircuit(WDFDEVICE Device,
                                            ACXCIRCUIT *Circuit) {
  return QpwgraphCreateCircuit(Device, FALSE, QpwgraphAppCable, Circuit);
}

static NTSTATUS QpwgraphCreateMonitorCircuit(WDFDEVICE Device,
                                             ACXCIRCUIT *Circuit) {
  return QpwgraphCreateCircuit(Device, TRUE, QpwgraphAppCable, Circuit);
}

static NTSTATUS QpwgraphCreateRelayRenderCircuit(WDFDEVICE Device,
                                                 ACXCIRCUIT *Circuit) {
  return QpwgraphCreateCircuit(Device, FALSE, QpwgraphRelayCable, Circuit);
}

static NTSTATUS QpwgraphCreateRelayCaptureCircuit(WDFDEVICE Device,
                                                  ACXCIRCUIT *Circuit) {
  return QpwgraphCreateCircuit(Device, TRUE, QpwgraphRelayCable, Circuit);
}

static NTSTATUS
QpwgraphEvtDevicePrepareHardware(WDFDEVICE Device, WDFCMRESLIST ResourcesRaw,
                                 WDFCMRESLIST ResourcesTranslated) {
  PQPWGRAPH_DEVICE_CONTEXT Context;
  NTSTATUS status;

  UNREFERENCED_PARAMETER(ResourcesRaw);
  UNREFERENCED_PARAMETER(ResourcesTranslated);
  PAGED_CODE();
  Context = QpwgraphGetDeviceContext(Device);
  if (Context == NULL || Context->RenderCircuit == NULL ||
      Context->MonitorCircuit == NULL || Context->RelayRenderCircuit == NULL ||
      Context->RelayCaptureCircuit == NULL) {
    return STATUS_INVALID_DEVICE_STATE;
  }
  if (Context->RenderCircuitAdded && Context->MonitorCircuitAdded &&
      Context->RelayRenderCircuitAdded && Context->RelayCaptureCircuitAdded) {
    return STATUS_SUCCESS;
  }
  if (!Context->RenderCircuitAdded) {
    status = AcxDeviceAddCircuit(Device, Context->RenderCircuit);
    if (!NT_SUCCESS(status)) {
      goto rollback;
    }
    Context->RenderCircuitAdded = TRUE;
  }
  if (!Context->MonitorCircuitAdded) {
    status = AcxDeviceAddCircuit(Device, Context->MonitorCircuit);
    if (!NT_SUCCESS(status)) {
      goto rollback;
    }
    Context->MonitorCircuitAdded = TRUE;
  }
  if (!Context->RelayRenderCircuitAdded) {
    status = AcxDeviceAddCircuit(Device, Context->RelayRenderCircuit);
    if (!NT_SUCCESS(status)) {
      goto rollback;
    }
    Context->RelayRenderCircuitAdded = TRUE;
  }
  if (!Context->RelayCaptureCircuitAdded) {
    status = AcxDeviceAddCircuit(Device, Context->RelayCaptureCircuit);
    if (!NT_SUCCESS(status)) {
      goto rollback;
    }
    Context->RelayCaptureCircuitAdded = TRUE;
  }
  return STATUS_SUCCESS;

rollback:
  if (Context->RelayCaptureCircuitAdded) {
    if (NT_SUCCESS(
            AcxDeviceRemoveCircuit(Device, Context->RelayCaptureCircuit))) {
      Context->RelayCaptureCircuitAdded = FALSE;
    }
  }
  if (Context->RelayRenderCircuitAdded) {
    if (NT_SUCCESS(
            AcxDeviceRemoveCircuit(Device, Context->RelayRenderCircuit))) {
      Context->RelayRenderCircuitAdded = FALSE;
    }
  }
  if (Context->MonitorCircuitAdded) {
    if (NT_SUCCESS(AcxDeviceRemoveCircuit(Device, Context->MonitorCircuit))) {
      Context->MonitorCircuitAdded = FALSE;
    }
  }
  if (Context->RenderCircuitAdded) {
    if (NT_SUCCESS(AcxDeviceRemoveCircuit(Device, Context->RenderCircuit))) {
      Context->RenderCircuitAdded = FALSE;
    }
  }
  return status;
}

static NTSTATUS
QpwgraphEvtDeviceReleaseHardware(WDFDEVICE Device,
                                 WDFCMRESLIST ResourcesTranslated) {
  PQPWGRAPH_DEVICE_CONTEXT Context;
  NTSTATUS status;
  NTSTATUS circuitStatus;

  UNREFERENCED_PARAMETER(ResourcesTranslated);
  PAGED_CODE();
  Context = QpwgraphGetDeviceContext(Device);
  if (Context == NULL || Context->RenderCircuit == NULL ||
      Context->MonitorCircuit == NULL || Context->RelayRenderCircuit == NULL ||
      Context->RelayCaptureCircuit == NULL) {
    return STATUS_INVALID_DEVICE_STATE;
  }
  status = STATUS_SUCCESS;
  if (Context->RelayCaptureCircuitAdded) {
    circuitStatus =
        AcxDeviceRemoveCircuit(Device, Context->RelayCaptureCircuit);
    if (NT_SUCCESS(circuitStatus)) {
      Context->RelayCaptureCircuitAdded = FALSE;
    }
    if (NT_SUCCESS(status)) {
      status = circuitStatus;
    }
  }
  if (Context->RelayRenderCircuitAdded) {
    circuitStatus = AcxDeviceRemoveCircuit(Device, Context->RelayRenderCircuit);
    if (NT_SUCCESS(circuitStatus)) {
      Context->RelayRenderCircuitAdded = FALSE;
    }
    if (NT_SUCCESS(status)) {
      status = circuitStatus;
    }
  }
  if (Context->MonitorCircuitAdded) {
    circuitStatus = AcxDeviceRemoveCircuit(Device, Context->MonitorCircuit);
    if (NT_SUCCESS(circuitStatus)) {
      Context->MonitorCircuitAdded = FALSE;
    }
    if (NT_SUCCESS(status)) {
      status = circuitStatus;
    }
  }
  if (Context->RenderCircuitAdded) {
    circuitStatus = AcxDeviceRemoveCircuit(Device, Context->RenderCircuit);
    if (NT_SUCCESS(circuitStatus)) {
      Context->RenderCircuitAdded = FALSE;
    }
    if (NT_SUCCESS(status)) {
      status = circuitStatus;
    }
  }
  qpwgraph_audio_transport_clear();
  qpwgraph_audio_transport_clear_relay();
  return status;
}

NTSTATUS qpwgraph_acx_device_add(void *driver, void *device_init) {
  ACX_DEVICEINIT_CONFIG deviceInitConfig;
  ACX_DEVICE_CONFIG deviceConfig;
  WDF_PNPPOWER_EVENT_CALLBACKS pnpCallbacks;
  WDF_OBJECT_ATTRIBUTES attributes;
  PWDFDEVICE_INIT deviceInit;
  WDFDEVICE device;
  PQPWGRAPH_DEVICE_CONTEXT Context;
  NTSTATUS status;

  UNREFERENCED_PARAMETER(driver);
  PAGED_CODE();
  if (device_init == NULL) {
    return STATUS_INVALID_PARAMETER;
  }

  deviceInit = (PWDFDEVICE_INIT)device_init;

  ACX_DEVICEINIT_CONFIG_INIT(&deviceInitConfig);
  status = AcxDeviceInitInitialize(deviceInit, &deviceInitConfig);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnpCallbacks);
  pnpCallbacks.EvtDevicePrepareHardware = QpwgraphEvtDevicePrepareHardware;
  pnpCallbacks.EvtDeviceReleaseHardware = QpwgraphEvtDeviceReleaseHardware;
  WdfDeviceInitSetPnpPowerEventCallbacks(deviceInit, &pnpCallbacks);

  WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, QPWGRAPH_DEVICE_CONTEXT);
  status = WdfDeviceCreate(&deviceInit, &attributes, &device);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  ACX_DEVICE_CONFIG_INIT(&deviceConfig);
  status = AcxDeviceInitialize(device, &deviceConfig);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  Context = QpwgraphGetDeviceContext(device);
  RtlZeroMemory(Context, sizeof(*Context));
  status = QpwgraphCreateRenderCircuit(device, &Context->RenderCircuit);
  if (!NT_SUCCESS(status)) {
    Context->RenderCircuit = NULL;
    return status;
  }
  status = QpwgraphCreateMonitorCircuit(device, &Context->MonitorCircuit);
  if (!NT_SUCCESS(status)) {
    Context->MonitorCircuit = NULL;
    Context->RenderCircuit = NULL;
    return status;
  }
  status =
      QpwgraphCreateRelayRenderCircuit(device, &Context->RelayRenderCircuit);
  if (!NT_SUCCESS(status)) {
    Context->RelayRenderCircuit = NULL;
    Context->MonitorCircuit = NULL;
    Context->RenderCircuit = NULL;
    return status;
  }
  status =
      QpwgraphCreateRelayCaptureCircuit(device, &Context->RelayCaptureCircuit);
  if (!NT_SUCCESS(status)) {
    Context->RelayCaptureCircuit = NULL;
    Context->RelayRenderCircuit = NULL;
    Context->MonitorCircuit = NULL;
    Context->RenderCircuit = NULL;
    return status;
  }
  qpwgraph_audio_transport_clear();
  qpwgraph_audio_transport_clear_relay();
  return STATUS_SUCCESS;
}
