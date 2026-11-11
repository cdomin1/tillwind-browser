#include <string.h>
#include <unistd.h>
#include <webkit/webkit-web-process-extension.h>

static gboolean handle_message(WebKitWebPage *page, WebKitUserMessage *message,
                               gpointer data)
{
    (void)page;
    (void)data;
    if (g_strcmp0(webkit_user_message_get_name(message), "tillwind-process") != 0)
        return FALSE;

    /* The PID can be namespaced. The kernel start time survives the sandbox. */
    g_autofree char *stat = NULL;
    guint64 start_time = 0;
    if (g_file_get_contents("/proc/self/stat", &stat, NULL, NULL)) {
        const char *comm_end = strrchr(stat, ')');
        if (comm_end != NULL && comm_end[1] == ' ') {
            g_auto(GStrv) fields = g_strsplit(comm_end + 2, " ", 0);
            if (g_strv_length(fields) > 19)
                start_time = g_ascii_strtoull(fields[19], NULL, 10);
        }
    }

    WebKitUserMessage *reply = webkit_user_message_new(
        "tillwind-process", g_variant_new("(ut)", (guint32)getpid(), start_time));
    webkit_user_message_send_reply(message, reply);
    return TRUE;
}

static void page_created(WebKitWebProcessExtension *extension, WebKitWebPage *page,
                         gpointer data)
{
    (void)extension;
    (void)data;
    g_signal_connect(page, "user-message-received", G_CALLBACK(handle_message), NULL);
}

G_MODULE_EXPORT void
webkit_web_process_extension_initialize(WebKitWebProcessExtension *extension)
{
    g_signal_connect(extension, "page-created", G_CALLBACK(page_created), NULL);
}
