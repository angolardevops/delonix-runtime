package httpapi

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strconv"

	"__NAME__/internal/health"
	"__NAME__/internal/notes"
	"__NAME__/internal/webhook"
)

type handlers struct{ d Deps }

// errorBody is the one error shape every endpoint answers with.
type errorBody struct {
	Error struct {
		Code      string            `json:"code"`
		Message   string            `json:"message"`
		Fields    map[string]string `json:"fields,omitempty"`
		RequestID string            `json:"request_id,omitempty"`
	} `json:"error"`
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeError(w http.ResponseWriter, r *http.Request, status int, code, msg string, fields map[string]string) {
	var b errorBody
	b.Error.Code, b.Error.Message, b.Error.Fields = code, msg, fields
	b.Error.RequestID = RequestIDFrom(r.Context())
	writeJSON(w, status, b)
}

func (h *handlers) live(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]string{"status": "alive"})
}

func (h *handlers) ready(w http.ResponseWriter, r *http.Request) {
	st, failing := h.d.Readiness.Evaluate(r.Context())
	switch {
	case st != health.Ready:
		writeJSON(w, http.StatusServiceUnavailable, map[string]string{"status": st.String()})
	case failing != "":
		h.d.Log.WarnContext(r.Context(), "readiness dependency failing", "dependency", failing)
		writeJSON(w, http.StatusServiceUnavailable, map[string]string{"status": "unavailable", "dependency": failing})
	default:
		writeJSON(w, http.StatusOK, map[string]string{"status": "ready"})
	}
}

// decode reads a JSON body strictly: unknown fields and trailing data are
// errors, and an oversized body answers 413.
func decode(w http.ResponseWriter, r *http.Request, dst any) bool {
	dec := json.NewDecoder(r.Body)
	dec.DisallowUnknownFields()
	err := dec.Decode(dst)
	if err == nil && dec.More() {
		err = errors.New("trailing data after the JSON object")
	}
	if err != nil {
		var tooBig *http.MaxBytesError
		if errors.As(err, &tooBig) {
			writeError(w, r, http.StatusRequestEntityTooLarge, "body_too_large", "request body too large", nil)
		} else {
			writeError(w, r, http.StatusBadRequest, "malformed_json", "request body is not the expected JSON", nil)
		}
		return false
	}
	return true
}

// useCaseError maps use-case errors to HTTP; anything unexpected is logged
// and answered as a bare 500.
func (h *handlers) useCaseError(w http.ResponseWriter, r *http.Request, err error) {
	var verr *notes.ValidationError
	switch {
	case errors.As(err, &verr):
		writeError(w, r, http.StatusUnprocessableEntity, "validation_failed", "invalid input", verr.Fields)
	case errors.Is(err, notes.ErrNotFound):
		writeError(w, r, http.StatusNotFound, "not_found", "note not found", nil)
	default:
		h.d.Log.ErrorContext(r.Context(), "use case failed", "error", err.Error())
		writeError(w, r, http.StatusInternalServerError, "internal", "internal error", nil)
	}
}

type createNoteRequest struct {
	Title string `json:"title"`
	Body  string `json:"body"`
}

func (h *handlers) createNote(w http.ResponseWriter, r *http.Request) {
	var req createNoteRequest
	if !decode(w, r, &req) {
		return
	}
	n, err := h.d.Notes.Create(r.Context(), notes.CreateInput{Title: req.Title, Body: req.Body})
	if err != nil {
		h.useCaseError(w, r, err)
		return
	}
	w.Header().Set("Location", "/api/v1/notes/"+n.ID)
	writeJSON(w, http.StatusCreated, n)
}

func (h *handlers) getNote(w http.ResponseWriter, r *http.Request) {
	n, err := h.d.Notes.Get(r.Context(), r.PathValue("id"))
	if err != nil {
		h.useCaseError(w, r, err)
		return
	}
	writeJSON(w, http.StatusOK, n)
}

func (h *handlers) listNotes(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	offset, limit := 0, 20
	fields := map[string]string{}
	if v := q.Get("offset"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil {
			fields["offset"] = "must be an integer"
		}
		offset = n
	}
	if v := q.Get("limit"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil {
			fields["limit"] = "must be an integer"
		}
		limit = n
	}
	if len(fields) > 0 {
		writeError(w, r, http.StatusUnprocessableEntity, "validation_failed", "invalid input", fields)
		return
	}
	page, err := h.d.Notes.List(r.Context(), offset, limit)
	if err != nil {
		h.useCaseError(w, r, err)
		return
	}
	writeJSON(w, http.StatusOK, page)
}

// inboundWebhook accepts a signed delivery. `note.create` runs the same use
// case as POST /api/v1/notes; `ping` only proves the signature. A delivery
// id already accepted is acknowledged again without being processed again.
func (h *handlers) inboundWebhook(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(io.LimitReader(r.Body, webhookMaxBody+1))
	if err != nil {
		var tooBig *http.MaxBytesError
		if errors.As(err, &tooBig) {
			writeError(w, r, http.StatusRequestEntityTooLarge, "body_too_large", "request body too large", nil)
			return
		}
		writeError(w, r, http.StatusBadRequest, "unreadable_body", "request body could not be read", nil)
		return
	}
	if len(body) > webhookMaxBody {
		writeError(w, r, http.StatusRequestEntityTooLarge, "body_too_large", "request body too large", nil)
		return
	}
	id := r.Header.Get(webhook.HeaderID)
	err = webhook.Verify(h.d.WebhookKey, id,
		r.Header.Get(webhook.HeaderTimestamp), r.Header.Get(webhook.HeaderSignature), body, h.d.Now())
	switch {
	case errors.Is(err, webhook.ErrMissingHeaders):
		writeError(w, r, http.StatusBadRequest, "missing_signature", "webhook headers are missing", nil)
		return
	case err != nil:
		h.d.Log.WarnContext(r.Context(), "webhook rejected", "reason", err.Error())
		writeError(w, r, http.StatusUnauthorized, "invalid_signature", "webhook signature is not valid", nil)
		return
	}
	if !h.d.Dedup.FirstSeen(id, h.d.Now()) {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	var ev struct {
		Type string          `json:"type"`
		Data json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(body, &ev); err != nil {
		writeError(w, r, http.StatusBadRequest, "malformed_json", "webhook body is not JSON", nil)
		return
	}
	switch ev.Type {
	case "ping":
	case "note.create":
		var in createNoteRequest
		if err := json.Unmarshal(ev.Data, &in); err != nil {
			writeError(w, r, http.StatusBadRequest, "malformed_json", "webhook data is not a note", nil)
			return
		}
		if _, err := h.d.Notes.Create(r.Context(), notes.CreateInput{Title: in.Title, Body: in.Body}); err != nil {
			// Not processed: a retry of this id must be processed, not
			// acknowledged as a duplicate.
			h.d.Dedup.Forget(id)
			h.useCaseError(w, r, err)
			return
		}
	default:
		writeError(w, r, http.StatusUnprocessableEntity, "unknown_event", "unknown webhook event type", nil)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// webhookMaxBody bounds an inbound delivery independently of the API limit.
const webhookMaxBody = 256 << 10
