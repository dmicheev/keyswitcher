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
#include <stdint.h>
#include <math.h>

/* Таймаут обмена сообщениями с AX, сек. Защита от зависания на «глухих» приложениях. */
static const double kAXTimeout = 0.25;
/* Таймаут запроса границ каретки: вызывается из конвейера печати,
 * где каждая миллисекунда заметна, поэтому заметно короче основного. */
static const double kCaretAXTimeout = 0.1;

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

/*
 * Выделенный текст фокусного элемента процесса pid (kAXSelectedTextAttribute).
 * Возвращает malloc'ную UTF-8 строку (освободить через ax_bridge_free)
 * либо NULL, если выделения нет / недоступно (нет прав, «глухое» приложение).
 */
char *ax_bridge_copy_selected_text(pid_t pid) {
    AXUIElementRef app = AXUIElementCreateApplication(pid);
    if (app == NULL) {
        return NULL;
    }
    AXUIElementSetMessagingTimeout(app, (Float32)kAXTimeout);

    AXValueRef winValue = NULL;
    AXError err = AXUIElementCopyAttributeValue(
        app, kAXFocusedWindowAttribute, (CFTypeRef *)&winValue);
    CFRelease(app);
    if (err != kAXErrorSuccess || winValue == NULL) {
        return NULL;
    }

    /* Фокусный элемент внутри окна (текстовое поле с выделением). */
    AXUIElementRef element = NULL;
    err = AXUIElementCopyAttributeValue(
        (AXUIElementRef)winValue, kAXFocusedUIElementAttribute,
        (CFTypeRef *)&element);
    CFRelease(winValue);
    if (err != kAXErrorSuccess || element == NULL) {
        return NULL;
    }

    AXValueRef textValue = NULL;
    err = AXUIElementCopyAttributeValue(
        element, kAXSelectedTextAttribute, (CFTypeRef *)&textValue);
    CFRelease(element);
    if (err != kAXErrorSuccess || textValue == NULL) {
        return NULL;
    }

    char *result = NULL;
    if (CFGetTypeID(textValue) == CFStringGetTypeID()) {
        result = cfstring_to_utf8_cstr((CFStringRef)textValue);
    }
    CFRelease(textValue);
    return result;
}

/*
 * Экранные границы каретки (или выделения) фокусного текстового элемента
 * процесса pid: kAXSelectedTextRangeAttribute + kAXBoundsForRange.
 * Координаты — «экранные» AX/CG (origin — левый верхний угол главного
 * дисплея, y растёт вниз, единицы — поинты).
 * Возвращает 1 и заполняет x/y/w/h, либо 0 (нет прав, приложение не
 * поддерживает boundsForRange, таймаут).
 */
int ax_bridge_copy_caret_bounds(
    pid_t pid,
    double *out_x, double *out_y, double *out_w, double *out_h) {
    if (out_x == NULL || out_y == NULL || out_w == NULL || out_h == NULL) {
        return 0;
    }
    AXUIElementRef app = AXUIElementCreateApplication(pid);
    if (app == NULL) {
        return 0;
    }
    AXUIElementSetMessagingTimeout(app, (Float32)kCaretAXTimeout);

    /* Фокусный элемент сразу у приложения (фокусное окно не нужно). */
    AXUIElementRef element = NULL;
    AXError err = AXUIElementCopyAttributeValue(
        app, kAXFocusedUIElementAttribute, (CFTypeRef *)&element);
    CFRelease(app);
    if (err != kAXErrorSuccess || element == NULL) {
        return 0;
    }

    /* Диапазон каретки/выделения (AXValue типа Range). */
    AXValueRef rangeValue = NULL;
    err = AXUIElementCopyAttributeValue(
        element, kAXSelectedTextRangeAttribute, (CFTypeRef *)&rangeValue);
    if (err != kAXErrorSuccess || rangeValue == NULL ||
        CFGetTypeID(rangeValue) != AXValueGetTypeID()) {
        CFRelease(element);
        return 0;
    }
    CFRange range = {0, 0};
    Boolean ok = AXValueGetValue(rangeValue, kAXValueCFRangeType, &range);
    if (!ok || range.location < 0 || range.length < 0) {
        CFRelease(rangeValue);
        CFRelease(element);
        return 0;
    }

    /* Экранные границы диапазона (AXValue типа CGRect). */
    AXValueRef boundsValue = NULL;
    err = AXUIElementCopyParameterizedAttributeValue(
        element, kAXBoundsForRangeParameterizedAttribute,
        (CFTypeRef)rangeValue, (CFTypeRef *)&boundsValue);
    CFRelease(rangeValue);
    CFRelease(element);
    if (err != kAXErrorSuccess || boundsValue == NULL) {
        return 0;
    }

    CGRect rect = CGRectZero;
    ok = AXValueGetValue(boundsValue, kAXValueCGRectType, &rect);
    CFRelease(boundsValue);
    /* У каретки ширина нулевая — проверяем только высоту и конечность. */
    if (!ok || !isfinite(rect.origin.x) || !isfinite(rect.origin.y) ||
        !isfinite(rect.size.width) || !isfinite(rect.size.height) ||
        rect.size.height <= 0.0 || rect.size.width < 0.0) {
        return 0;
    }
    *out_x = (double)rect.origin.x;
    *out_y = (double)rect.origin.y;
    *out_w = (double)rect.size.width;
    *out_h = (double)rect.size.height;
    return 1;
}

/*
 * Текущая позиция указателя мыши в экранных координатах CG/AX
 * (origin — левый верхний угол главного дисплея, y растёт вниз).
 * Возвращает 1 и заполняет x/y, либо 0.
 */
int ax_bridge_mouse_location(double *out_x, double *out_y) {
    if (out_x == NULL || out_y == NULL) {
        return 0;
    }
    CGEventRef event = CGEventCreate(NULL);
    if (event == NULL) {
        return 0;
    }
    CGPoint p = CGEventGetLocation(event);
    CFRelease(event);
    *out_x = (double)p.x;
    *out_y = (double)p.y;
    return 1;
}

/*
 * Геометрия дисплеев в тех же «экранных» координатах AX/CG (origin — левый
 * верхний угол ГЛАВНОГО дисплея, y растёт вниз; единицы — поинты).
 * Находит дисплей, содержащий точку (px, py), и возвращает:
 *   *out_flip_h — высота главного (primary) дисплея: по ней переводится y
 *                 из AX (от верха) в AppKit (от низа) — для любой точки;
 *   *out_dx..*out_dh — bounds найденного дисплея (кламп панели).
 * 1 — успех, 0 — список дисплеев недоступен.
 */
#define MAX_DISPLAYS 16
int ax_bridge_display_geometry(
    double px, double py,
    double *out_flip_h,
    double *out_dx, double *out_dy, double *out_dw, double *out_dh) {
    if (out_flip_h == NULL || out_dx == NULL || out_dy == NULL ||
        out_dw == NULL || out_dh == NULL) {
        return 0;
    }
    CGDirectDisplayID ids[MAX_DISPLAYS];
    uint32_t count = 0;
    if (CGGetActiveDisplayList(MAX_DISPLAYS, ids, &count) != kCGErrorSuccess ||
        count == 0) {
        return 0;
    }

    /* Главный дисплей — origin глобальных координат (0,0). */
    CGRect primary = CGDisplayBounds(CGMainDisplayID());
    *out_flip_h = (double)CGRectGetHeight(primary);

    /* Дисплей, содержащий точку; иначе — ближайший по центрам. */
    CGRect best = primary;
    double best_dist = -1.0;
    for (uint32_t i = 0; i < count; i++) {
        CGRect b = CGDisplayBounds(ids[i]);
        if (px >= CGRectGetMinX(b) && px <= CGRectGetMaxX(b) &&
            py >= CGRectGetMinY(b) && py <= CGRectGetMaxY(b)) {
            best = b;
            break;
        }
        double cx = CGRectGetMidX(b) - px;
        double cy = CGRectGetMidY(b) - py;
        double dist = cx * cx + cy * cy;
        if (best_dist < 0.0 || dist < best_dist) {
            best = b;
            best_dist = dist;
        }
    }
    *out_dx = (double)CGRectGetMinX(best);
    *out_dy = (double)CGRectGetMinY(best);
    *out_dw = (double)CGRectGetWidth(best);
    *out_dh = (double)CGRectGetHeight(best);
    return 1;
}
