#ifndef AGWFP_ABI_H
#define AGWFP_ABI_H

#define AGWFP_DEVICE_TYPE FILE_DEVICE_NETWORK
#define AGWFP_IOCTL_INDEX 0x800

#define AGWFP_IOCTL_SET_CONFIGURATION \
    CTL_CODE(AGWFP_DEVICE_TYPE, AGWFP_IOCTL_INDEX + 1, METHOD_BUFFERED, FILE_WRITE_DATA)

#define AGWFP_CONFIGURATION_VERSION_1 1u
#define AGWFP_FLOW_KIND_NATIVE 1u

typedef struct AGWFP_INET_ENDPOINT_V1_ {
    unsigned short family;
    unsigned short port; /* host byte order */
    union {
        unsigned int ipv4; /* host byte order */
        unsigned char ipv6[16];
    } address;
    unsigned int scope_id;
    unsigned char reserved[8];
} AGWFP_INET_ENDPOINT_V1;

typedef struct AGWFP_CONFIGURATION_V1_ {
    unsigned int version;
    unsigned int size;
    unsigned int live_service_pid;
    unsigned int flags;
    AGWFP_INET_ENDPOINT_V1 public_destination;
    AGWFP_INET_ENDPOINT_V1 proxy_destination;
} AGWFP_CONFIGURATION_V1;

#pragma pack(push, 1)
typedef struct AGWFP_REDIRECT_CONTEXT_HEADER_ {
    unsigned char magic[4];
    unsigned short version;
    unsigned short flow_kind;
    unsigned short sockaddr_len;
    unsigned short sid_len;
    unsigned int reserved;
} AGWFP_REDIRECT_CONTEXT_HEADER;
#pragma pack(pop)

#define AGWFP_REDIRECT_CONTEXT_MAGIC_0 'A'
#define AGWFP_REDIRECT_CONTEXT_MAGIC_1 'G'
#define AGWFP_REDIRECT_CONTEXT_MAGIC_2 'W'
#define AGWFP_REDIRECT_CONTEXT_MAGIC_3 'F'
#define AGWFP_REDIRECT_CONTEXT_VERSION_1 1u

#endif