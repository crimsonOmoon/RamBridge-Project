#include <jni.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <unistd.h>
#include <cstdint>
#include <atomic>

static uint8_t* g_ram_buffer = nullptr;
static size_t g_buffer_size = 0;
static std::atomic<bool> g_running{false};
static int g_server_fd = -1;

constexpr uint8_t CMD_READ  = 0x01;
constexpr uint8_t CMD_WRITE = 0x02;

#pragma pack(push, 1)
struct BlockHeader {
    uint8_t cmd;
    uint64_t offset;
    uint32_t length;
};
#pragma pack(pop)

bool recv_all(int sock, void* buf, size_t len) {
    uint8_t* p = static_cast<uint8_t*>(buf);
    while (len > 0) {
        ssize_t n = recv(sock, p, len, 0);
        if (n <= 0) return false;
        p += n;
        len -= n;
    }
    return true;
}

bool send_all(int sock, const void* buf, size_t len) {
    const uint8_t* p = static_cast<const uint8_t*>(buf);
    while (len > 0) {
        ssize_t n = send(sock, p, len, MSG_NOSIGNAL);
        if (n <= 0) return false;
        p += n;
        len -= n;
    }
    return true;
}

extern "C" JNIEXPORT jboolean JNICALL
Java_com_rambridge_app_MainActivity_allocateRamBuffer(JNIEnv*, jobject, jlong size_bytes) {
    if (g_ram_buffer != nullptr) return JNI_TRUE;

    // Lazy anonymous mmap (does not trigger Android Low-Memory Killer)
    void* ptr = mmap(nullptr, static_cast<size_t>(size_bytes),
                     PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    if (ptr == MAP_FAILED) {
        return JNI_FALSE;
    }

    g_ram_buffer = static_cast<uint8_t*>(ptr);
    g_buffer_size = static_cast<size_t>(size_bytes);
    return JNI_TRUE;
}

extern "C" JNIEXPORT void JNICALL
Java_com_rambridge_app_MainActivity_startBlockServer(JNIEnv*, jobject, jint port) {
    if (g_running) return;

    g_server_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (g_server_fd < 0) return;

    int opt = 1;
    setsockopt(g_server_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(static_cast<uint16_t>(port));

    if (bind(g_server_fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        close(g_server_fd);
        g_server_fd = -1;
        return;
    }
    if (listen(g_server_fd, 1) < 0) {
        close(g_server_fd);
        g_server_fd = -1;
        return;
    }

    g_running = true;
    while (g_running) {
        int client_fd = accept(g_server_fd, nullptr, nullptr);
        if (client_fd < 0) continue;

        setsockopt(client_fd, IPPROTO_TCP, TCP_NODELAY, &opt, sizeof(opt));

        BlockHeader hdr{};
        while (g_running && recv_all(client_fd, &hdr, sizeof(BlockHeader))) {
            if (g_ram_buffer == nullptr || hdr.offset + hdr.length > g_buffer_size) break;

            if (hdr.cmd == CMD_READ) {
                if (!send_all(client_fd, g_ram_buffer + hdr.offset, hdr.length)) break;
            } else if (hdr.cmd == CMD_WRITE) {
                if (!recv_all(client_fd, g_ram_buffer + hdr.offset, hdr.length)) break;
                uint8_t ack = 0x00;
                if (!send_all(client_fd, &ack, 1)) break;
            }
        }
        close(client_fd);
    }
}

extern "C" JNIEXPORT void JNICALL
Java_com_rambridge_app_MainActivity_freeRamBuffer(JNIEnv*, jobject) {
    g_running = false;
    if (g_server_fd >= 0) {
        shutdown(g_server_fd, SHUT_RDWR);
        close(g_server_fd);
        g_server_fd = -1;
    }
    if (g_ram_buffer != nullptr) {
        munmap(g_ram_buffer, g_buffer_size);
        g_ram_buffer = nullptr;
        g_buffer_size = 0;
    }
}