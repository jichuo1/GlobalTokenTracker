/* globaltokentracker-ffi — C ABI over the Rust core engine.
 *
 * Every function is synchronous and returns a heap UTF-8 JSON envelope:
 *   {"ok":true,"data":...}  or  {"ok":false,"error":"..."}
 * Release every returned pointer with gtt_free(). Call from a background
 * queue: gtt_load with scan, gtt_refresh_prices and gtt_poll_quotas can
 * take seconds. Requests are JSON; NULL means "defaults".
 */
#ifndef GTT_FFI_H
#define GTT_FFI_H

#ifdef __cplusplus
extern "C" {
#endif

/* {range:{key,start_ms?,end_ms?}, apps?, models?, devices?, scan, force_views, rebuild, force_prices}
 * devices: device ids to include (absent = the saved filter, default all).
 * vm.devices = this device + paired peers with per-device span totals;
 * vm.quotas_all_devices / vm.device_warnings; detail rows are this device's. */
char *gtt_load(const char *req_json);
/* LAN peers (sync / pair / discover use the network — background queue):
 *   {"op":"list"}                                   → {device, peers, warnings, filter}
 *   {"op":"sync","if_due"?:bool,"interval_secs"?,"rediscover"?:bool} → {ran, report}
 *   {"op":"set_filter","devices":[ids]|null}         → {filter}
 *   {"op":"forget","device":"id|name|fingerprint"}   → {forgotten}
 *   {"op":"set_enabled","device":"…","enabled":bool} → {device_id, enabled}
 *   {"op":"pair","addr":"host:port","code":"123456","name"?} → {device}
 *   {"op":"discover","timeout_secs"?:n}              → {devices, invalid_answers}
 * Sharing this device (TLS on 0.0.0.0:4319 + mDNS; one share per process):
 *   {"op":"share_start","name"?}  {"op":"share_stop"}  {"op":"share_status"}
 *   {"op":"share_new_code"}       {"op":"revoke","viewer":"id|name"}
 *   → {sharing, status?:{device_id, device_name, fingerprint, bind, port,
 *       advertised, code?, code_expires_in?}, viewers:[{id,name,created_at,
 *       last_seen_at,revoked_at}]}
 * Protocol failures: {"ok":false,"error":"<code>","code":"<code>"} with the
 * fixed codes of docs/PEER_API.md (pairing_failed, peer_fingerprint_mismatch …). */
char *gtt_peer(const char *req_json);
/* {offset, limit?, apps?, models?} */
char *gtt_detail_page(const char *req_json);
char *gtt_sources(void);
/* {query, disputed_only, reload} → {rows (with "disputed"), total, synced_at} */
char *gtt_prices(const char *req_json);
/* -1 = read, 0 / 1 = set → {enabled, invalid, error?, path} */
char *gtt_external_interface(int set);
/* Display currency for money figures → {display, usd_to, source?,
 * fetched_at_ms?, stale}. gtt_set_display_currency takes an ISO code
 * ("CNY"); NULL or "" restores the ledger default (USD). */
char *gtt_fx_status(void);
char *gtt_refresh_fx(void);
char *gtt_set_display_currency(const char *code);
/* Download + SHA-256-verify the update for "stable" | "alpha" → {version, path} */
char *gtt_download_update(const char *channel);
char *gtt_refresh_prices(void);
char *gtt_poll_quotas(void);
char *gtt_watch_roots(void);
/* Custom statistics window (pure time math, safe on the main thread):
 *   {"op":"days","first":"YYYY-MM-DD","last":"YYYY-MM-DD"}
 *   {"op":"hours","day":"YYYY-MM-DD","h1":0..23,"h2":0..23}
 *   {"op":"quick","key":"yesterday|this_week|this_month|last_month|last_90_days"}
 *   {"op":"describe","range":{key,start_ms?,end_ms?}}
 * → {range, first_day?, last_day?, days?, hour_span?:[a,b], hourly, today, now_hour} */
char *gtt_range(const char *req_json);
/* "stable" | "alpha"; NULL = stable */
char *gtt_check_update(const char *channel);
char *gtt_info(void);
void gtt_free(char *s);

#ifdef __cplusplus
}
#endif

#endif
