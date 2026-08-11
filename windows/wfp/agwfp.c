#include <ntifs.h>
#include <ndis.h>
#include <fwpsk.h>
#include <fwpmk.h>
#include <fwptypes.h>
#include <in6addr.h>
#include <initguid.h>
#include <ntintsafe.h>
#include <ntstrsafe.h>
#include <wdmsec.h>
#include <ws2def.h>

#include "agwfp_abi.h"

#define AGWFP_POOL_TAG 'fWGA'

#define AGWFP_DEVICE_NAME L"\\Device\\AGWfp"
#define AGWFP_SYMBOLIC_NAME L"\\DosDevices\\Global\\AGWfp"
#define AGWFP_DEVICE_SDDL L"D:P(A;;GA;;;SY)(A;;GA;;;BA)"

DEFINE_GUID(AGWFP_DEVICE_CLASS_GUID,
    0x5e8130fd, 0xac8d, 0x4928, 0xb0, 0xa8, 0x19, 0x3a, 0x40, 0xd3, 0x31, 0x33);
DEFINE_GUID(AGWFP_PROVIDER_GUID,
    0xd3d96c97, 0x45cc, 0x464b, 0x85, 0x6f, 0xfd, 0x4f, 0x0e, 0x3b, 0xf8, 0x34);
DEFINE_GUID(AGWFP_SUBLAYER_GUID,
    0xf5c3028a, 0x0c62, 0x466c, 0x83, 0x91, 0xb0, 0x88, 0xf8, 0xcd, 0x45, 0x80);
DEFINE_GUID(AGWFP_CALLOUT_V4_GUID,
    0xca0a1d1b, 0xf38b, 0x42fa, 0x8b, 0x12, 0x3f, 0xd2, 0x1f, 0xf5, 0x4a, 0x20);
DEFINE_GUID(AGWFP_CALLOUT_V6_GUID,
    0xb3f02491, 0x2371, 0x4bd2, 0xaf, 0x96, 0x6f, 0x85, 0x35, 0x30, 0xa8, 0x92);

C_ASSERT(sizeof(AGWFP_REDIRECT_CONTEXT_HEADER) == 16);

typedef struct AGWFP_RUNTIME_ {
    volatile LONG references;
    KEVENT zero_references;
    AGWFP_CONFIGURATION_V1 config;
    PEPROCESS service_process;
    SOCKADDR_STORAGE public_destination;
    SOCKADDR_STORAGE proxy_destination;
    USHORT public_sockaddr_length;
    USHORT proxy_sockaddr_length;
    HANDLE engine_handle;
    HANDLE redirect_handle;
    UINT32 engine_callout_id_v4;
    UINT32 engine_callout_id_v6;
    UINT64 filter_id_v4;
    UINT64 filter_id_v6;
    BOOLEAN provider_added;
    BOOLEAN sublayer_added;
    BOOLEAN callout_added_v4;
    BOOLEAN callout_added_v6;
    BOOLEAN filter_added_v4;
    BOOLEAN filter_added_v6;
} AGWFP_RUNTIME;

typedef struct AGWFP_GLOBALS_ {
    PDEVICE_OBJECT device_object;
    UNICODE_STRING symbolic_name;
    KSPIN_LOCK runtime_lock;
    KMUTEX configuration_mutex;
    AGWFP_RUNTIME* runtime;
    UINT32 registered_callout_id_v4;
    UINT32 registered_callout_id_v6;
} AGWFP_GLOBALS;

static AGWFP_GLOBALS g_agwfp;

DRIVER_INITIALIZE DriverEntry;

static VOID NTAPI AgwfpClassify(
    _In_ const FWPS_INCOMING_VALUES0* in_fixed_values,
    _In_ const FWPS_INCOMING_METADATA_VALUES0* in_meta_values,
    _Inout_opt_ VOID* layer_data,
    _In_opt_ const VOID* classify_context,
    _In_ const FWPS_FILTER1* filter,
    _In_ UINT64 flow_context,
    _Inout_ FWPS_CLASSIFY_OUT0* classify_out);
static NTSTATUS NTAPI AgwfpNotify(
    _In_ FWPS_CALLOUT_NOTIFY_TYPE notify_type,
    _In_ const GUID* filter_key,
    _Inout_ FWPS_FILTER1* filter);
static VOID NTAPI AgwfpFlowDelete(
    _In_ UINT16 layer_id,
    _In_ UINT32 callout_id,
    _In_ UINT64 flow_context);

static VOID AgwfpSetBlock(_Inout_ FWPS_CLASSIFY_OUT0* classify_out)
{
    classify_out->actionType = FWP_ACTION_BLOCK;
    classify_out->flags |= FWPS_CLASSIFY_OUT_FLAG_ABSORB;
}

static VOID AgwfpSetPermit(_Inout_ FWPS_CLASSIFY_OUT0* classify_out)
{
    classify_out->actionType = FWP_ACTION_PERMIT;
}

static BOOLEAN AgwfpIsIpv4Loopback(UINT32 address)
{
    return address == 0x7F000001u;
}

static BOOLEAN AgwfpIsIpv6Loopback(_In_reads_(16) const UCHAR address[16])
{
    static const UCHAR loopback[16] = { 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1 };

    return RtlCompareMemory(address, loopback, sizeof(loopback)) == sizeof(loopback);
}

static USHORT AgwfpSockaddrLength(ADDRESS_FAMILY family)
{
    switch (family) {
    case AF_INET:
        return (USHORT)sizeof(SOCKADDR_IN);
    case AF_INET6:
        return (USHORT)sizeof(SOCKADDR_IN6);
    default:
        return 0;
    }
}

static NTSTATUS AgwfpEndpointToSockaddr(
    _In_ const AGWFP_INET_ENDPOINT_V1* endpoint,
    _Out_ SOCKADDR_STORAGE* storage,
    _Out_ USHORT* storage_length)
{
    RtlZeroMemory(storage, sizeof(*storage));

    switch (endpoint->family) {
    case AF_INET:
    {
        SOCKADDR_IN* ipv4 = (SOCKADDR_IN*)storage;

        if (endpoint->scope_id != 0) {
            return STATUS_INVALID_PARAMETER;
        }

        ipv4->sin_family = AF_INET;
        ipv4->sin_port = RtlUshortByteSwap(endpoint->port);
        ipv4->sin_addr.S_un.S_addr = RtlUlongByteSwap(endpoint->address.ipv4);
        *storage_length = (USHORT)sizeof(*ipv4);
        return STATUS_SUCCESS;
    }
    case AF_INET6:
    {
        SOCKADDR_IN6* ipv6 = (SOCKADDR_IN6*)storage;

        ipv6->sin6_family = AF_INET6;
        ipv6->sin6_port = RtlUshortByteSwap(endpoint->port);
        ipv6->sin6_scope_id = endpoint->scope_id;
        RtlCopyMemory(&ipv6->sin6_addr, endpoint->address.ipv6, sizeof(endpoint->address.ipv6));
        *storage_length = (USHORT)sizeof(*ipv6);
        return STATUS_SUCCESS;
    }
    default:
        return STATUS_INVALID_PARAMETER;
    }
}

static NTSTATUS AgwfpValidateEndpoint(_In_ const AGWFP_INET_ENDPOINT_V1* endpoint)
{
    if (endpoint->port == 0) {
        return STATUS_INVALID_PARAMETER;
    }

    if (!RtlIsZeroMemory((VOID*)endpoint->reserved, sizeof(endpoint->reserved))) {
        return STATUS_INVALID_PARAMETER;
    }

    switch (endpoint->family) {
    case AF_INET:
        if (!AgwfpIsIpv4Loopback(endpoint->address.ipv4)) {
            return STATUS_INVALID_ADDRESS;
        }
        return STATUS_SUCCESS;
    case AF_INET6:
        if (!AgwfpIsIpv6Loopback(endpoint->address.ipv6)) {
            return STATUS_INVALID_ADDRESS;
        }
        return STATUS_SUCCESS;
    default:
        return STATUS_INVALID_PARAMETER;
    }
}

static NTSTATUS AgwfpValidateConfiguration(
    _In_ const AGWFP_CONFIGURATION_V1* config,
    _Out_ SOCKADDR_STORAGE* public_destination,
    _Out_ USHORT* public_length,
    _Out_ SOCKADDR_STORAGE* proxy_destination,
    _Out_ USHORT* proxy_length)
{
    PEPROCESS process = NULL;
    NTSTATUS status;

    if (config->version != AGWFP_CONFIGURATION_VERSION_1 ||
        config->size != sizeof(*config) ||
        config->flags != 0 ||
        config->live_service_pid == 0) {
        return STATUS_INVALID_PARAMETER;
    }

    if (config->public_destination.family != config->proxy_destination.family) {
        return STATUS_INVALID_PARAMETER_MIX;
    }

    if (RtlCompareMemory(
            &config->public_destination,
            &config->proxy_destination,
            sizeof(config->public_destination)) == sizeof(config->public_destination)) {
        return STATUS_INVALID_PARAMETER_MIX;
    }

    status = AgwfpValidateEndpoint(&config->public_destination);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = AgwfpValidateEndpoint(&config->proxy_destination);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = PsLookupProcessByProcessId(ULongToHandle(config->live_service_pid), &process);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = AgwfpEndpointToSockaddr(&config->public_destination, public_destination, public_length);
    if (!NT_SUCCESS(status)) {
        ObDereferenceObject(process);
        return status;
    }

    status = AgwfpEndpointToSockaddr(&config->proxy_destination, proxy_destination, proxy_length);
    if (!NT_SUCCESS(status)) {
        ObDereferenceObject(process);
        return status;
    }

    ObDereferenceObject(process);

    return STATUS_SUCCESS;
}

static AGWFP_RUNTIME* AgwfpCreateRuntime(
    _In_ const AGWFP_CONFIGURATION_V1* config,
    _In_ const SOCKADDR_STORAGE* public_destination,
    _In_ USHORT public_length,
    _In_ const SOCKADDR_STORAGE* proxy_destination,
    _In_ USHORT proxy_length)
{
    AGWFP_RUNTIME* runtime = ExAllocatePoolZero(NonPagedPoolNx, sizeof(*runtime), AGWFP_POOL_TAG);

    if (runtime == NULL) {
        return NULL;
    }

    runtime->references = 1;
    KeInitializeEvent(&runtime->zero_references, NotificationEvent, FALSE);
    runtime->config = *config;
    runtime->public_destination = *public_destination;
    runtime->proxy_destination = *proxy_destination;
    runtime->public_sockaddr_length = public_length;
    runtime->proxy_sockaddr_length = proxy_length;
    if (!NT_SUCCESS(PsLookupProcessByProcessId(
            ULongToHandle(config->live_service_pid),
            &runtime->service_process))) {
        ExFreePoolWithTag(runtime, AGWFP_POOL_TAG);
        return NULL;
    }
    return runtime;
}

static VOID AgwfpRuntimeDereference(_In_ AGWFP_RUNTIME* runtime)
{
    if (InterlockedDecrement(&runtime->references) == 0) {
        KeSetEvent(&runtime->zero_references, IO_NO_INCREMENT, FALSE);
    }
}

static AGWFP_RUNTIME* AgwfpRuntimeReference(VOID)
{
    AGWFP_RUNTIME* runtime;
    KIRQL old_irql;

    KeAcquireSpinLock(&g_agwfp.runtime_lock, &old_irql);
    runtime = g_agwfp.runtime;
    if (runtime != NULL) {
        InterlockedIncrement(&runtime->references);
    }
    KeReleaseSpinLock(&g_agwfp.runtime_lock, old_irql);
    return runtime;
}

static AGWFP_RUNTIME* AgwfpSwapRuntime(_In_opt_ AGWFP_RUNTIME* runtime)
{
    AGWFP_RUNTIME* previous;
    KIRQL old_irql;

    KeAcquireSpinLock(&g_agwfp.runtime_lock, &old_irql);
    previous = g_agwfp.runtime;
    g_agwfp.runtime = runtime;
    KeReleaseSpinLock(&g_agwfp.runtime_lock, old_irql);
    return previous;
}

static VOID AgwfpDeleteEngineObjects(_Inout_ AGWFP_RUNTIME* runtime)
{
    if (runtime->engine_handle == NULL) {
        return;
    }

    if (runtime->filter_added_v4) {
        (VOID)FwpmFilterDeleteById0(runtime->engine_handle, runtime->filter_id_v4);
        runtime->filter_added_v4 = FALSE;
    }
    if (runtime->filter_added_v6) {
        (VOID)FwpmFilterDeleteById0(runtime->engine_handle, runtime->filter_id_v6);
        runtime->filter_added_v6 = FALSE;
    }
    if (runtime->callout_added_v4) {
        (VOID)FwpmCalloutDeleteById0(runtime->engine_handle, runtime->engine_callout_id_v4);
        runtime->callout_added_v4 = FALSE;
    }
    if (runtime->callout_added_v6) {
        (VOID)FwpmCalloutDeleteById0(runtime->engine_handle, runtime->engine_callout_id_v6);
        runtime->callout_added_v6 = FALSE;
    }
    if (runtime->sublayer_added) {
        (VOID)FwpmSubLayerDeleteByKey0(runtime->engine_handle, &AGWFP_SUBLAYER_GUID);
        runtime->sublayer_added = FALSE;
    }
    if (runtime->provider_added) {
        (VOID)FwpmProviderDeleteByKey0(runtime->engine_handle, &AGWFP_PROVIDER_GUID);
        runtime->provider_added = FALSE;
    }
}

static VOID AgwfpDestroyRuntime(_In_opt_ AGWFP_RUNTIME* runtime)
{
    if (runtime == NULL) {
        return;
    }

    AgwfpDeleteEngineObjects(runtime);

    AgwfpRuntimeDereference(runtime);
    (VOID)KeWaitForSingleObject(&runtime->zero_references, Executive, KernelMode, FALSE, NULL);

    if (runtime->redirect_handle != NULL) {
        FwpsRedirectHandleDestroy0(runtime->redirect_handle);
        runtime->redirect_handle = NULL;
    }
    if (runtime->engine_handle != NULL) {
        (VOID)FwpmEngineClose0(runtime->engine_handle);
        runtime->engine_handle = NULL;
    }

    if (runtime->service_process != NULL) {
        ObDereferenceObject(runtime->service_process);
        runtime->service_process = NULL;
    }

    ExFreePoolWithTag(runtime, AGWFP_POOL_TAG);
}

static NTSTATUS AgwfpAddCalloutAndFilter(
    _Inout_ AGWFP_RUNTIME* runtime,
    _In_ ADDRESS_FAMILY family,
    _In_ BOOLEAN add_filter)
{
    FWPM_CALLOUT0 callout;
    FWPM_FILTER_CONDITION0 conditions[3];
    FWPM_FILTER0 filter;
    FWP_BYTE_ARRAY16 byte_array16;
    NTSTATUS status;

    RtlZeroMemory(&callout, sizeof(callout));
    callout.calloutKey = family == AF_INET ? AGWFP_CALLOUT_V4_GUID : AGWFP_CALLOUT_V6_GUID;
    callout.providerKey = (GUID*)&AGWFP_PROVIDER_GUID;
    callout.applicableLayer = family == AF_INET ? FWPM_LAYER_ALE_CONNECT_REDIRECT_V4 : FWPM_LAYER_ALE_CONNECT_REDIRECT_V6;
    callout.displayData.name = family == AF_INET ? L"Agent Desktop WFP Redirect V4" : L"Agent Desktop WFP Redirect V6";

    status = FwpmCalloutAdd0(
        runtime->engine_handle,
        &callout,
        NULL,
        family == AF_INET ? &runtime->engine_callout_id_v4 : &runtime->engine_callout_id_v6);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    if (family == AF_INET) {
        runtime->callout_added_v4 = TRUE;
    } else {
        runtime->callout_added_v6 = TRUE;
    }

    if (!add_filter) {
        return STATUS_SUCCESS;
    }

    RtlZeroMemory(conditions, sizeof(conditions));
    conditions[0].fieldKey = FWPM_CONDITION_IP_PROTOCOL;
    conditions[0].matchType = FWP_MATCH_EQUAL;
    conditions[0].conditionValue.type = FWP_UINT8;
    conditions[0].conditionValue.uint8 = IPPROTO_TCP;

    conditions[1].fieldKey = FWPM_CONDITION_IP_REMOTE_PORT;
    conditions[1].matchType = FWP_MATCH_EQUAL;
    conditions[1].conditionValue.type = FWP_UINT16;
    conditions[1].conditionValue.uint16 = runtime->config.public_destination.port;

    conditions[2].fieldKey = FWPM_CONDITION_IP_REMOTE_ADDRESS;
    conditions[2].matchType = FWP_MATCH_EQUAL;
    if (family == AF_INET) {
        conditions[2].conditionValue.type = FWP_UINT32;
        conditions[2].conditionValue.uint32 = runtime->config.public_destination.address.ipv4;
    } else {
        const SOCKADDR_IN6* public6 = (const SOCKADDR_IN6*)&runtime->public_destination;

        RtlZeroMemory(&byte_array16, sizeof(byte_array16));
        RtlCopyMemory(byte_array16.byteArray16, &public6->sin6_addr, sizeof(byte_array16.byteArray16));
        conditions[2].conditionValue.type = FWP_BYTE_ARRAY16_TYPE;
        conditions[2].conditionValue.byteArray16 = &byte_array16;
    }

    RtlZeroMemory(&filter, sizeof(filter));
    filter.providerKey = (GUID*)&AGWFP_PROVIDER_GUID;
    filter.subLayerKey = AGWFP_SUBLAYER_GUID;
    filter.layerKey = family == AF_INET ? FWPM_LAYER_ALE_CONNECT_REDIRECT_V4 : FWPM_LAYER_ALE_CONNECT_REDIRECT_V6;
    filter.displayData.name = family == AF_INET ? L"Agent Desktop Redirect Filter V4" : L"Agent Desktop Redirect Filter V6";
    filter.action.type = FWP_ACTION_CALLOUT_TERMINATING;
    filter.action.calloutKey = callout.calloutKey;
    filter.weight.type = FWP_EMPTY;
    filter.numFilterConditions = RTL_NUMBER_OF(conditions);
    filter.filterCondition = conditions;

    status = FwpmFilterAdd0(
        runtime->engine_handle,
        &filter,
        NULL,
        family == AF_INET ? &runtime->filter_id_v4 : &runtime->filter_id_v6);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    if (family == AF_INET) {
        runtime->filter_added_v4 = TRUE;
    } else {
        runtime->filter_added_v6 = TRUE;
    }

    return STATUS_SUCCESS;
}

static NTSTATUS AgwfpActivateRuntime(_Inout_ AGWFP_RUNTIME* runtime)
{
    FWPM_PROVIDER0 provider;
    FWPM_SESSION0 session;
    FWPM_SUBLAYER0 sublayer;
    AGWFP_RUNTIME* displaced;
    ADDRESS_FAMILY active_family;
    NTSTATUS status;

    RtlZeroMemory(&session, sizeof(session));
    session.displayData.name = L"Agent Desktop WFP Driver Session";
    status = FwpmEngineOpen0(NULL, RPC_C_AUTHN_DEFAULT, NULL, &session, &runtime->engine_handle);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    RtlZeroMemory(&provider, sizeof(provider));
    provider.providerKey = AGWFP_PROVIDER_GUID;
    provider.displayData.name = L"Agent Desktop WFP Provider";
    provider.serviceName = L"AGWfp";
    status = FwpmProviderAdd0(runtime->engine_handle, &provider, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    runtime->provider_added = TRUE;

    RtlZeroMemory(&sublayer, sizeof(sublayer));
    sublayer.subLayerKey = AGWFP_SUBLAYER_GUID;
    sublayer.providerKey = (GUID*)&AGWFP_PROVIDER_GUID;
    sublayer.displayData.name = L"Agent Desktop WFP Redirect Sublayer";
    sublayer.weight = 0x5000;
    status = FwpmSubLayerAdd0(runtime->engine_handle, &sublayer, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    runtime->sublayer_added = TRUE;

    active_family = (ADDRESS_FAMILY)runtime->config.public_destination.family;

    status = AgwfpAddCalloutAndFilter(runtime, AF_INET, active_family == AF_INET);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = AgwfpAddCalloutAndFilter(runtime, AF_INET6, active_family == AF_INET6);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = FwpsRedirectHandleCreate0(&AGWFP_PROVIDER_GUID, 0, &runtime->redirect_handle);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    displaced = AgwfpSwapRuntime(runtime);
    NT_ASSERT(displaced == NULL);

    return STATUS_SUCCESS;
}

static VOID AgwfpRemoveRuntime(VOID)
{
    AGWFP_RUNTIME* runtime = AgwfpSwapRuntime(NULL);

    AgwfpDestroyRuntime(runtime);
}

static NTSTATUS AgwfpReplaceConfiguration(_In_ const AGWFP_CONFIGURATION_V1* config)
{
    SOCKADDR_STORAGE public_destination;
    SOCKADDR_STORAGE proxy_destination;
    USHORT public_length;
    USHORT proxy_length;
    AGWFP_RUNTIME* runtime;
    AGWFP_RUNTIME* existing;
    NTSTATUS status;

    existing = AgwfpRuntimeReference();
    if (existing != NULL) {
        AgwfpRuntimeDereference(existing);
        return STATUS_ALREADY_REGISTERED;
    }

    status = AgwfpValidateConfiguration(
        config,
        &public_destination,
        &public_length,
        &proxy_destination,
        &proxy_length);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    runtime = AgwfpCreateRuntime(config, &public_destination, public_length, &proxy_destination, proxy_length);
    if (runtime == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    status = AgwfpActivateRuntime(runtime);
    if (!NT_SUCCESS(status)) {
        AgwfpDestroyRuntime(runtime);
    }

    return status;
}

static NTSTATUS AgwfpRegisterCallouts(_In_ PDEVICE_OBJECT device_object)
{
    FWPS_CALLOUT1 callout;
    NTSTATUS status;

    RtlZeroMemory(&callout, sizeof(callout));
    callout.classifyFn = AgwfpClassify;
    callout.notifyFn = AgwfpNotify;
    callout.flowDeleteFn = AgwfpFlowDelete;

    callout.calloutKey = AGWFP_CALLOUT_V4_GUID;
    status = FwpsCalloutRegister1(device_object, &callout, &g_agwfp.registered_callout_id_v4);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    callout.calloutKey = AGWFP_CALLOUT_V6_GUID;
    status = FwpsCalloutRegister1(device_object, &callout, &g_agwfp.registered_callout_id_v6);
    if (!NT_SUCCESS(status)) {
        FwpsCalloutUnregisterById0(g_agwfp.registered_callout_id_v4);
        g_agwfp.registered_callout_id_v4 = 0;
        return status;
    }

    return STATUS_SUCCESS;
}

static VOID AgwfpUnregisterCallouts(VOID)
{
    if (g_agwfp.registered_callout_id_v6 != 0) {
        FwpsCalloutUnregisterById0(g_agwfp.registered_callout_id_v6);
        g_agwfp.registered_callout_id_v6 = 0;
    }
    if (g_agwfp.registered_callout_id_v4 != 0) {
        FwpsCalloutUnregisterById0(g_agwfp.registered_callout_id_v4);
        g_agwfp.registered_callout_id_v4 = 0;
    }
}

static NTSTATUS AgwfpBuildRedirectContext(
    _In_ const SOCKADDR_STORAGE* original_destination,
    _In_ USHORT sockaddr_length,
    _In_ PSID owner_sid,
    _Outptr_result_bytebuffer_(*context_size) VOID** context,
    _Out_ UINT32* context_size)
{
    AGWFP_REDIRECT_CONTEXT_HEADER* header;
    SIZE_T sid_length;
    SIZE_T total_size;
    NTSTATUS status;

    sid_length = RtlLengthSid(owner_sid);
    status = RtlSizeTAdd(sizeof(*header), sockaddr_length, &total_size);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = RtlSizeTAdd(total_size, sid_length, &total_size);
    if (!NT_SUCCESS(status) || total_size > MAXUINT32) {
        return STATUS_INTEGER_OVERFLOW;
    }

    header = ExAllocatePoolZero(NonPagedPoolNx, total_size, AGWFP_POOL_TAG);
    if (header == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    header->magic[0] = AGWFP_REDIRECT_CONTEXT_MAGIC_0;
    header->magic[1] = AGWFP_REDIRECT_CONTEXT_MAGIC_1;
    header->magic[2] = AGWFP_REDIRECT_CONTEXT_MAGIC_2;
    header->magic[3] = AGWFP_REDIRECT_CONTEXT_MAGIC_3;
    header->version = AGWFP_REDIRECT_CONTEXT_VERSION_1;
    header->flow_kind = AGWFP_FLOW_KIND_NATIVE;
    header->sockaddr_len = sockaddr_length;
    header->sid_len = (USHORT)sid_length;
    header->reserved = 0;

    RtlCopyMemory((UCHAR*)header + sizeof(*header), original_destination, sockaddr_length);
    status = RtlCopySid((ULONG)sid_length, (UCHAR*)header + sizeof(*header) + sockaddr_length, owner_sid);
    if (!NT_SUCCESS(status)) {
        ExFreePoolWithTag(header, AGWFP_POOL_TAG);
        return status;
    }

    *context = header;
    *context_size = (UINT32)total_size;
    return STATUS_SUCCESS;
}

static NTSTATUS AgwfpExtractUserSid(
    _In_ const FWPS_INCOMING_METADATA_VALUES0* metadata,
    _Outptr_ PSID* user_sid,
    _Outptr_ PTOKEN_USER* token_user)
{
    ULONG required_length = 0;
    NTSTATUS status;

    *token_user = NULL;
    if (metadata == NULL ||
        !FWPS_IS_METADATA_FIELD_PRESENT(metadata, FWPS_METADATA_FIELD_TOKEN) ||
        metadata->token == 0) {
        return STATUS_NO_TOKEN;
    }

    status = ZwQueryInformationToken(
        (HANDLE)(ULONG_PTR)metadata->token,
        TokenUser,
        NULL,
        0,
        &required_length);
    if (status != STATUS_BUFFER_TOO_SMALL || required_length < sizeof(TOKEN_USER)) {
        return NT_SUCCESS(status) ? STATUS_INVALID_BUFFER_SIZE : status;
    }

    *token_user = ExAllocatePoolZero(PagedPool, required_length, AGWFP_POOL_TAG);
    if (*token_user == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    status = ZwQueryInformationToken(
        (HANDLE)(ULONG_PTR)metadata->token,
        TokenUser,
        *token_user,
        required_length,
        &required_length);
    if (!NT_SUCCESS(status)) {
        ExFreePoolWithTag(*token_user, AGWFP_POOL_TAG);
        *token_user = NULL;
        return status;
    }

    *user_sid = (*token_user)->User.Sid;
    if (*user_sid == NULL || !RtlValidSid(*user_sid)) {
        ExFreePoolWithTag(*token_user, AGWFP_POOL_TAG);
        *token_user = NULL;
        return STATUS_INVALID_SID;
    }

    return STATUS_SUCCESS;
}

static BOOLEAN AgwfpOriginalDestinationMatches(
    _In_ UINT16 layer_id,
    _In_ const FWPS_INCOMING_VALUES0* in_fixed_values,
    _In_ const AGWFP_RUNTIME* runtime)
{
    if (layer_id == FWPS_LAYER_ALE_CONNECT_REDIRECT_V4) {
        const SOCKADDR_IN* public4 = (const SOCKADDR_IN*)&runtime->public_destination;
        const FWPS_INCOMING_VALUE0* address = &in_fixed_values->incomingValue[FWPS_FIELD_ALE_CONNECT_REDIRECT_V4_IP_REMOTE_ADDRESS];
        const FWPS_INCOMING_VALUE0* port = &in_fixed_values->incomingValue[FWPS_FIELD_ALE_CONNECT_REDIRECT_V4_IP_REMOTE_PORT];

        return address->value.uint32 == RtlUlongByteSwap(public4->sin_addr.S_un.S_addr) &&
            port->value.uint16 == RtlUshortByteSwap(public4->sin_port);
    }

    if (layer_id == FWPS_LAYER_ALE_CONNECT_REDIRECT_V6) {
        const SOCKADDR_IN6* public6 = (const SOCKADDR_IN6*)&runtime->public_destination;
        const FWPS_INCOMING_VALUE0* address = &in_fixed_values->incomingValue[FWPS_FIELD_ALE_CONNECT_REDIRECT_V6_IP_REMOTE_ADDRESS];
        const FWPS_INCOMING_VALUE0* port = &in_fixed_values->incomingValue[FWPS_FIELD_ALE_CONNECT_REDIRECT_V6_IP_REMOTE_PORT];

        if (address->value.byteArray16 == NULL) {
            return FALSE;
        }

        return RtlCompareMemory(
            address->value.byteArray16->byteArray16,
            &public6->sin6_addr,
            sizeof(address->value.byteArray16->byteArray16)) == sizeof(address->value.byteArray16->byteArray16) &&
            port->value.uint16 == RtlUshortByteSwap(public6->sin6_port);
    }

    return FALSE;
}

static VOID NTAPI AgwfpClassify(
    _In_ const FWPS_INCOMING_VALUES0* in_fixed_values,
    _In_ const FWPS_INCOMING_METADATA_VALUES0* in_meta_values,
    _Inout_opt_ VOID* layer_data,
    _In_opt_ const VOID* classify_context,
    _In_ const FWPS_FILTER1* filter,
    _In_ UINT64 flow_context,
    _Inout_ FWPS_CLASSIFY_OUT0* classify_out)
{
    AGWFP_RUNTIME* runtime = NULL;
    FWPS_CONNECTION_REDIRECT_STATE redirect_state = FWPS_CONNECTION_NOT_REDIRECTED;
    FWPS_CONNECT_REQUEST0* connect_request;
    VOID* writable_layer_data = NULL;
    VOID* redirect_context = NULL;
    UINT32 redirect_context_size = 0;
    UINT64 classify_handle = 0;
    PSID owner_sid = NULL;
    PTOKEN_USER token_user = NULL;
    USHORT sockaddr_length;
    NTSTATUS status;

    UNREFERENCED_PARAMETER(flow_context);
    if ((classify_out->rights & FWPS_RIGHT_ACTION_WRITE) == 0 ||
        layer_data == NULL ||
        classify_context == NULL ||
        filter == NULL) {
        AgwfpSetBlock(classify_out);
        return;
    }

    runtime = AgwfpRuntimeReference();
    if (runtime == NULL) {
        AgwfpSetBlock(classify_out);
        return;
    }

    if (PsGetProcessExitStatus(runtime->service_process) != STATUS_PENDING) {
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    if (in_meta_values != NULL &&
        FWPS_IS_METADATA_FIELD_PRESENT(in_meta_values, FWPS_METADATA_FIELD_REDIRECT_RECORD_HANDLE)) {
        redirect_state = FwpsQueryConnectionRedirectState0(
            in_meta_values->redirectRecords,
            runtime->redirect_handle,
            NULL);
    }

    if (redirect_state != FWPS_CONNECTION_NOT_REDIRECTED) {
        AgwfpSetPermit(classify_out);
        goto Exit;
    }

    if (!AgwfpOriginalDestinationMatches(in_fixed_values->layerId, in_fixed_values, runtime)) {
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    status = AgwfpExtractUserSid(in_meta_values, &owner_sid, &token_user);
    if (!NT_SUCCESS(status)) {
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    status = FwpsAcquireClassifyHandle0((VOID*)classify_context, 0, &classify_handle);
    if (!NT_SUCCESS(status)) {
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    status = FwpsAcquireWritableLayerDataPointer0(
        classify_handle,
        filter->filterId,
        0,
        &writable_layer_data,
        classify_out);
    if (!NT_SUCCESS(status)) {
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    connect_request = (FWPS_CONNECT_REQUEST0*)writable_layer_data;
    if (connect_request->previousVersion != NULL) {
        if (connect_request->previousVersion->modifierFilterId == filter->filterId ||
            connect_request->previousVersion->localRedirectHandle != NULL) {
            FwpsApplyModifiedLayerData0(classify_handle, writable_layer_data, 0);
            AgwfpSetPermit(classify_out);
            goto Exit;
        }
    }

    sockaddr_length = AgwfpSockaddrLength(((SOCKADDR*)&connect_request->remoteAddressAndPort)->sa_family);
    if (sockaddr_length == 0) {
        (VOID)FwpsApplyModifiedLayerData0(classify_handle, writable_layer_data, 0);
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    status = AgwfpBuildRedirectContext(
        &connect_request->remoteAddressAndPort,
        sockaddr_length,
        owner_sid,
        &redirect_context,
        &redirect_context_size);
    if (!NT_SUCCESS(status)) {
        (VOID)FwpsApplyModifiedLayerData0(classify_handle, writable_layer_data, 0);
        AgwfpSetBlock(classify_out);
        goto Exit;
    }

    RtlZeroMemory(&connect_request->remoteAddressAndPort, sizeof(connect_request->remoteAddressAndPort));
    RtlCopyMemory(&connect_request->remoteAddressAndPort, &runtime->proxy_destination, runtime->proxy_sockaddr_length);
    connect_request->localRedirectTargetPID = runtime->config.live_service_pid;
    connect_request->localRedirectHandle = runtime->redirect_handle;
    connect_request->localRedirectContext = redirect_context;
    connect_request->localRedirectContextSize = redirect_context_size;

    FwpsApplyModifiedLayerData0(
        classify_handle,
        writable_layer_data,
        0);
    redirect_context = NULL;
    AgwfpSetPermit(classify_out);

Exit:
    if (classify_handle != 0) {
        FwpsReleaseClassifyHandle0(classify_handle);
    }
    if (redirect_context != NULL) {
        ExFreePoolWithTag(redirect_context, AGWFP_POOL_TAG);
    }
    if (token_user != NULL) {
        ExFreePoolWithTag(token_user, AGWFP_POOL_TAG);
    }
    if (runtime != NULL) {
        AgwfpRuntimeDereference(runtime);
    }
}

static NTSTATUS NTAPI AgwfpNotify(
    _In_ FWPS_CALLOUT_NOTIFY_TYPE notify_type,
    _In_ const GUID* filter_key,
    _Inout_ FWPS_FILTER1* filter)
{
    UNREFERENCED_PARAMETER(notify_type);
    UNREFERENCED_PARAMETER(filter_key);
    UNREFERENCED_PARAMETER(filter);
    return STATUS_SUCCESS;
}

static VOID NTAPI AgwfpFlowDelete(
    _In_ UINT16 layer_id,
    _In_ UINT32 callout_id,
    _In_ UINT64 flow_context)
{
    UNREFERENCED_PARAMETER(layer_id);
    UNREFERENCED_PARAMETER(callout_id);
    UNREFERENCED_PARAMETER(flow_context);
}

static NTSTATUS AgwfpCreateClose(_In_ PDEVICE_OBJECT device_object, _Inout_ PIRP irp)
{
    UNREFERENCED_PARAMETER(device_object);

    irp->IoStatus.Status = STATUS_SUCCESS;
    irp->IoStatus.Information = 0;
    IoCompleteRequest(irp, IO_NO_INCREMENT);
    return STATUS_SUCCESS;
}

static NTSTATUS AgwfpDeviceControl(_In_ PDEVICE_OBJECT device_object, _Inout_ PIRP irp)
{
    PIO_STACK_LOCATION stack = IoGetCurrentIrpStackLocation(irp);
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    ULONG_PTR information = 0;

    UNREFERENCED_PARAMETER(device_object);

    if (stack->Parameters.DeviceIoControl.IoControlCode == AGWFP_IOCTL_SET_CONFIGURATION) {
        if (stack->Parameters.DeviceIoControl.InputBufferLength != sizeof(AGWFP_CONFIGURATION_V1) ||
            irp->AssociatedIrp.SystemBuffer == NULL) {
            status = STATUS_BUFFER_TOO_SMALL;
        } else {
            (VOID)KeWaitForSingleObject(
                &g_agwfp.configuration_mutex,
                Executive,
                KernelMode,
                FALSE,
                NULL);
            status = AgwfpReplaceConfiguration((const AGWFP_CONFIGURATION_V1*)irp->AssociatedIrp.SystemBuffer);
            KeReleaseMutex(&g_agwfp.configuration_mutex, FALSE);
        }
    }

    irp->IoStatus.Status = status;
    irp->IoStatus.Information = information;
    IoCompleteRequest(irp, IO_NO_INCREMENT);
    return status;
}

static VOID AgwfpUnload(_In_ PDRIVER_OBJECT driver_object)
{
    UNREFERENCED_PARAMETER(driver_object);

    AgwfpRemoveRuntime();
    AgwfpUnregisterCallouts();

    if (g_agwfp.symbolic_name.Buffer != NULL) {
        (VOID)IoDeleteSymbolicLink(&g_agwfp.symbolic_name);
    }
    if (g_agwfp.device_object != NULL) {
        IoDeleteDevice(g_agwfp.device_object);
        g_agwfp.device_object = NULL;
    }
}

NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT driver_object, _In_ PUNICODE_STRING registry_path)
{
    UNICODE_STRING device_name = RTL_CONSTANT_STRING(AGWFP_DEVICE_NAME);
    UNICODE_STRING symbolic_name = RTL_CONSTANT_STRING(AGWFP_SYMBOLIC_NAME);
    UNICODE_STRING sddl = RTL_CONSTANT_STRING(AGWFP_DEVICE_SDDL);
    NTSTATUS status;
    UINT32 major_function;

    UNREFERENCED_PARAMETER(registry_path);

    RtlZeroMemory(&g_agwfp, sizeof(g_agwfp));
    KeInitializeSpinLock(&g_agwfp.runtime_lock);
    KeInitializeMutex(&g_agwfp.configuration_mutex, 0);
    g_agwfp.symbolic_name = symbolic_name;

    status = IoCreateDeviceSecure(
        driver_object,
        0,
        &device_name,
        AGWFP_DEVICE_TYPE,
        FILE_DEVICE_SECURE_OPEN,
        FALSE,
        &sddl,
        &AGWFP_DEVICE_CLASS_GUID,
        &g_agwfp.device_object);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    g_agwfp.device_object->Flags |= DO_BUFFERED_IO;

    status = IoCreateSymbolicLink(&g_agwfp.symbolic_name, &device_name);
    if (!NT_SUCCESS(status)) {
        IoDeleteDevice(g_agwfp.device_object);
        g_agwfp.device_object = NULL;
        return status;
    }

    for (major_function = 0; major_function <= IRP_MJ_MAXIMUM_FUNCTION; major_function++) {
        driver_object->MajorFunction[major_function] = AgwfpCreateClose;
    }
    driver_object->MajorFunction[IRP_MJ_DEVICE_CONTROL] = AgwfpDeviceControl;
    driver_object->DriverUnload = AgwfpUnload;

    status = AgwfpRegisterCallouts(g_agwfp.device_object);
    if (!NT_SUCCESS(status)) {
        (VOID)IoDeleteSymbolicLink(&g_agwfp.symbolic_name);
        IoDeleteDevice(g_agwfp.device_object);
        g_agwfp.device_object = NULL;
        return status;
    }

    g_agwfp.device_object->Flags &= ~DO_DEVICE_INITIALIZING;
    return STATUS_SUCCESS;
}