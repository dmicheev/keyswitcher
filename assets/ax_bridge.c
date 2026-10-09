/*
 * ax_bridge.c — минимальный мост к macOS Accessibility API.
 *
 * Возвращает заголовок фокусного окна процесса по его PID.
 * Компилируется build.rs только на macOS; вызывается из Rust
 * (см. src/macos/system.rs).
 *
 * Если пользователем НЕ выдано разрешение Accessibility,
 * AXUIElementSetMessagingTimeout гасит зависание, а функции
 * возвращают ошибку — мост честно отдаёт NULL.
 */

#include <ApplicationServices/ApplicationServices.h>
#include <CoreFoundation/CoreFoundation.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

/* Таймаут обмена сообщениями с AX, сек. Защита от зависания на «глухих» приложениях. */
static const double kAXTimeout = 0.25;

static char *cfstring_to_utf8_cstr(CFStringRef str) {
    if (str == NULL || CFGetTypeID(str) != CFStringGetTypeID()) {
        return NULL;
    }
    /* Быстрая дорожка */
    const char *ptr = CFStringGetCStringPtr(str, kCFStringEncodingUTF8);
    if (ptr != NULL) {
        return strdup(ptr);
    }
    /* Медленная дорожка */
    CFIndex length = CFStringGetLength(str);
    CFIndex maxSize = CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8) + 1;
    if (maxSize <= 0) {
        return NULL;
    }
    char *buf = (char *)malloc((size_t)maxSize);
    if (buf == NULL) {
        return NULL;
    }
    if (!CFStringGetCString(str, buf, maxSize, kCFStringEncodingUTF8)) {
        free(buf);
        return NULL;
    }
    return buf;
}

/*
 * Заголовок фокусного окна процесса pid.
 * Возвращает malloc'ную UTF-8 строку (освободить через ax_bridge_free)
 * либо NULL, если получить недоступно (нет прав / нет окна / таймаут).
 */
char *ax_bridge_copy_title(pid_t pid) {
    AXUIElementRef app = AXUIElementCreateApplication(pid);
    if (app == NULL) {
        return NULL;
    }
    AXUIElementSetMessagingTimeout(app, (Float32)kAXTimeout);

    AXValueRef winValue = NULL;
    AXError err = AXUIElementCopyAttributeValue(
        app, kAXFocusedWindowAttribute, (CFTypeRef *)&winValue);
    if (err != kAXErrorSuccess || winValue == NULL) {
        /* Откат на main window, если фокусного нет */
        err = AXUIElementCopyAttributeValue(
            app, kAXMainWindowAttribute, (CFTypeRef *)&winValue);
    }
    CFRelease(app);
    if (err != kAXErrorSuccess || winValue == NULL) {
        return NULL;
    }

    AXValueRef titleValue = NULL;
    err = AXUIElementCopyAttributeValue(
        (AXUIElementRef)winValue, kAXTitleAttribute, (CFTypeRef *)&titleValue);
    CFRelease(winValue);
    if (err != kAXErrorSuccess || titleValue == NULL) {
        return NULL;
    }

    char *result = cfstring_to_utf8_cstr((CFStringRef)titleValue);
    CFRelease(titleValue);
    return result;
}

void ax_bridge_free(const char *ptr) {
    free((void *)ptr);
}

/* Проверка: выдано ли приложению право Accessibility. 1 — да. */
int ax_bridge_is_trusted(void) {
    return AXIsProcessTrusted() ? 1 : 0;
}
