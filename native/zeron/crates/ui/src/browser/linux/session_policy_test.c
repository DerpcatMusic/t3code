// Run with cc and pkg-config's webkit2gtk-4.1/json-glib-1.0 flags. This
// exercises the helper's actual policy without initializing GTK, opening a
// browser, making network requests, or reading any user's browser profile.
#define main browser_helper_main
#include "helper.c"
#undef main

static void same_origin_bootstrap(void) {
    g_assert_true(t3_session_target_allowed("http://127.0.0.1:3773", "http://127.0.0.1:3773/settings/connections"));
    g_assert_true(t3_session_target_allowed("https://example.test/", "https://example.test/settings/providers"));
    g_assert_false(t3_session_target_allowed("https://example.test", "https://other.test/settings"));
    g_assert_false(t3_session_target_allowed("https://example.test", "http://example.test/settings"));
    g_assert_false(t3_session_target_allowed("http://localhost:3773", "http://localhost:3774/settings"));
}

static void credential_free_entry_url(void) {
    g_assert_false(t3_session_target_allowed("https://user:synthetic@example.test", "https://example.test/settings"));
    g_assert_false(t3_session_target_allowed("https://example.test", "https://user:synthetic@example.test/settings"));
    g_assert_false(t3_session_target_allowed("https://example.test/?token=synthetic", "https://example.test/settings"));
    g_assert_false(t3_session_target_allowed("https://example.test", "https://example.test/settings?token=synthetic"));
    g_assert_false(t3_session_target_allowed("https://example.test", "https://example.test/settings#synthetic"));
    g_assert_false(t3_session_target_allowed("file:///tmp", "file:///tmp/settings"));
    g_assert_false(t3_session_target_allowed("https://example.test/other", "https://example.test/settings"));
}

static void utility_page_titles(void) {
    g_assert_cmpstr(t3_page_title("http://localhost:3773/settings"), ==, "T3 Settings");
    g_assert_cmpstr(t3_page_title("http://localhost:3773/settings/connections"), ==, "T3 Connect");
    g_assert_cmpstr(t3_page_title("http://localhost:3773/settings/providers"), ==, "T3 Providers");
    g_assert_cmpstr(t3_page_title("http://localhost:3773/pull-requests"), ==, "Pull Requests");
    g_assert_cmpstr(t3_page_title("http://localhost:3773/usage"), ==, "Usage");
}

int main(int argc, char **argv) {
    g_test_init(&argc, &argv, NULL);
    g_test_add_func("/t3-session/same-origin-bootstrap", same_origin_bootstrap);
    g_test_add_func("/t3-session/credential-free-entry-url", credential_free_entry_url);
    g_test_add_func("/t3-session/utility-page-titles", utility_page_titles);
    return g_test_run();
}
