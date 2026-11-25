#include <enet/enet.h>
#include <stdio.h>
#include <arpa/inet.h>
#include <string.h>
#include <stdlib.h>
int main(int argc, char** argv) {
    int port = atoi(argv[1]);
    if (enet_initialize() != 0) return 1;
    ENetAddress address; memset(&address, 0, sizeof(address));
    const char* loopback = argc > 2 ? argv[2] : "127.0.0.1";
    int family = strchr(loopback, ':') ? AF_INET6 : AF_INET;
    if (family == AF_INET6) {
        struct sockaddr_in6 sin6 = { .sin6_family = AF_INET6 };
        if (inet_pton(family, loopback, &sin6.sin6_addr) != 1) return 1;
        enet_address_set_address(&address, (struct sockaddr*)&sin6, sizeof(sin6));
    } else {
        struct sockaddr_in sin = { .sin_family = AF_INET };
        if (inet_pton(family, loopback, &sin.sin_addr) != 1) return 1;
        enet_address_set_address(&address, (struct sockaddr*)&sin, sizeof(sin));
    }
    enet_address_set_port(&address, port);
    ENetHost* server = enet_host_create(family, &address, 4, 0x30, 0, 0);
    if (!server) { printf("host create failed\n"); return 1; }
    printf("READY\n"); fflush(stdout);
    ENetEvent event; int done = 0;
    while (!done && enet_host_service(server, &event, 15000) >= 0) {
        if (event.type == ENET_EVENT_TYPE_NONE) { printf("TIMEOUT\n"); break; }
        switch (event.type) {
        case ENET_EVENT_TYPE_CONNECT:
            printf("CONNECT data=%u\n", event.data); fflush(stdout);
            break;
        case ENET_EVENT_TYPE_RECEIVE: {
            printf("RECV ch=%d len=%zu data=%.*s\n", event.channelID, event.packet->dataLength, (int)event.packet->dataLength, (char*)event.packet->data); fflush(stdout);
            if (event.packet->dataLength == 3 && memcmp(event.packet->data, "bye", 3) == 0) {
                enet_peer_disconnect(event.peer, 42);
            } else if (event.packet->dataLength == 3 && memcmp(event.packet->data, "big", 3) == 0) {
                char big[5000]; for (int i = 0; i < 5000; i++) big[i] = 'a' + (i % 26);
                enet_peer_send(event.peer, event.channelID, enet_packet_create(big, sizeof(big), ENET_PACKET_FLAG_RELIABLE));
            } else {
                char reply[256]; int n = snprintf(reply, sizeof(reply), "echo:%.*s", (int)event.packet->dataLength, (char*)event.packet->data);
                enet_peer_send(event.peer, event.channelID, enet_packet_create(reply, n, ENET_PACKET_FLAG_RELIABLE));
                enet_peer_send(event.peer, event.channelID, enet_packet_create("unseq", 5, ENET_PACKET_FLAG_UNSEQUENCED));
            }
            enet_packet_destroy(event.packet);
            break; }
        case ENET_EVENT_TYPE_DISCONNECT:
            printf("DISCONNECT\n"); fflush(stdout); done = 1;
            break;
        default: break;
        }
    }
    enet_host_destroy(server);
    return 0;
}
