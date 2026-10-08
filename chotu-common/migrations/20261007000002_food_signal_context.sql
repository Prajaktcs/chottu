-- Associate confirmations and pending photo choices with their originating sender.
-- Meal consumption time remains in food_log; logged_at is interaction time only.
CREATE TABLE food_signal_context (
    food_log_id TEXT PRIMARY KEY REFERENCES food_log(id) ON DELETE CASCADE,
    recipient_kind TEXT NOT NULL,
    recipient_id TEXT NOT NULL,
    sender_aci TEXT NOT NULL,
    user_facts TEXT NOT NULL,
    logged_at TEXT NOT NULL
);
CREATE INDEX food_signal_context_sender ON food_signal_context(recipient_kind, recipient_id, sender_aci, logged_at);

CREATE TABLE food_signal_messages (
    recipient_kind TEXT NOT NULL,
    recipient_id TEXT NOT NULL,
    message_timestamp INTEGER NOT NULL,
    -- Retain deleted targets so an old reply cannot accidentally log a new meal.
    food_log_id TEXT NOT NULL,
    PRIMARY KEY (recipient_kind, recipient_id, message_timestamp)
);

CREATE TABLE food_photo_choices (
    recipient_kind TEXT NOT NULL,
    recipient_id TEXT NOT NULL,
    sender_aci TEXT NOT NULL,
    prompt_timestamp INTEGER NOT NULL,
    attachment_id TEXT NOT NULL,
    content_type TEXT NOT NULL,
    caption TEXT NOT NULL,
    candidates_json TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (recipient_kind, recipient_id, sender_aci)
);
