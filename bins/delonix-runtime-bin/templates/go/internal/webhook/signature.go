// Package webhook signs and verifies webhooks in the Standard Webhooks format
// (https://www.standardwebhooks.com): headers webhook-id, webhook-timestamp
// and webhook-signature, the signature being base64(HMAC-SHA256(key,
// "<id>.<timestamp>.<raw body>")) prefixed with "v1,". Using a published
// format lets a sender or receiver written in another language, or an
// existing library, talk to this service without reading its code.
package webhook

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"
)

// Header names (lower-case; HTTP headers are case-insensitive).
const (
	HeaderID        = "webhook-id"
	HeaderTimestamp = "webhook-timestamp"
	HeaderSignature = "webhook-signature"
)

// Tolerance is how far a timestamp may be from now, either way. Outside it a
// delivery is refused, which bounds how long a captured request can be
// replayed.
const Tolerance = 5 * time.Minute

// ParseSecret decodes a "whsec_<base64>" secret. The key must be at least 24
// bytes, the spec's lower bound.
func ParseSecret(s string) ([]byte, error) {
	raw, ok := strings.CutPrefix(s, "whsec_")
	if !ok {
		return nil, errors.New(`webhook secret must look like "whsec_<base64>"`)
	}
	key, err := base64.StdEncoding.DecodeString(raw)
	if err != nil {
		return nil, fmt.Errorf("webhook secret is not valid base64: %w", err)
	}
	if len(key) < 24 {
		return nil, errors.New("webhook secret must decode to at least 24 bytes")
	}
	return key, nil
}

// Sign returns the webhook-signature header value.
func Sign(key []byte, id string, ts time.Time, body []byte) string {
	mac := hmac.New(sha256.New, key)
	fmt.Fprintf(mac, "%s.%d.", id, ts.Unix())
	mac.Write(body)
	return "v1," + base64.StdEncoding.EncodeToString(mac.Sum(nil))
}

// Verification errors. They are distinct so the handler can answer 400 for a
// malformed request and 401 for a bad signature, but none of them says which
// byte was wrong.
var (
	ErrMissingHeaders = errors.New("missing webhook headers")
	ErrBadTimestamp   = errors.New("webhook timestamp outside the tolerance window")
	ErrBadSignature   = errors.New("no webhook signature matches")
)

// Verify checks a delivery over the RAW body bytes (never a re-encoded
// body: re-serializing JSON changes bytes and breaks every signature).
func Verify(key []byte, id, timestamp, signatures string, body []byte, now time.Time) error {
	if id == "" || timestamp == "" || signatures == "" {
		return ErrMissingHeaders
	}
	secs, err := strconv.ParseInt(timestamp, 10, 64)
	if err != nil {
		return ErrBadTimestamp
	}
	ts := time.Unix(secs, 0)
	if d := now.Sub(ts); d > Tolerance || d < -Tolerance {
		return ErrBadTimestamp
	}
	want := Sign(key, id, ts, body)
	// Several space-separated signatures are allowed (key rotation).
	for _, sig := range strings.Fields(signatures) {
		if hmac.Equal([]byte(sig), []byte(want)) {
			return nil
		}
	}
	return ErrBadSignature
}
