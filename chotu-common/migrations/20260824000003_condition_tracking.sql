-- Per-member condition tracking: closed food-tag vocabulary, write-time tags,
-- Telegram-managed watchlists, and daily 0–5 check-ins.
-- See docs/condition-tracking-spec.md (M1). Vocabulary changes ship as
-- forward migrations — do not edit this seed.

-- Fixed tag vocabulary. Seeded here; extendable by future migrations only.
CREATE TABLE IF NOT EXISTS food_tags (
    tag TEXT PRIMARY KEY,              -- e.g. 'alcohol'
    label TEXT NOT NULL,               -- e.g. 'Alcohol'
    description TEXT NOT NULL DEFAULT ''
);

-- Tags attached to a food log row at write time (same tx as the food insert).
-- Column comments are logical refs only; this repo does not declare SQLite FOREIGN KEYs.
CREATE TABLE IF NOT EXISTS food_log_tags (
    food_log_id TEXT NOT NULL,         -- food_log.id
    tag TEXT NOT NULL,                 -- food_tags.tag
    source TEXT NOT NULL DEFAULT 'llm', -- 'llm' | 'keyword' | 'manual'
    PRIMARY KEY (food_log_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_food_log_tags_tag ON food_log_tags (tag);

-- Per-member, per-condition watchlist (managed via /watch in Telegram).
CREATE TABLE IF NOT EXISTS condition_watchlist (
    family_member_id TEXT NOT NULL,
    condition_id TEXT NOT NULL,        -- matches config health_conditions.id
    tag TEXT NOT NULL,                 -- food_tags.tag
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (family_member_id, condition_id, tag)
);

-- Daily 0–5 symptom score captured during evening reflection.
CREATE TABLE IF NOT EXISTS condition_checkin (
    family_member_id TEXT NOT NULL,
    date TEXT NOT NULL,                -- YYYY-MM-DD local civil day
    condition_id TEXT NOT NULL,
    score INTEGER NOT NULL,            -- 0 = calm .. 5 = worst flare
    note TEXT,                         -- optional one-liner (itch / stress / ...)
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (family_member_id, date, condition_id)
);

INSERT OR IGNORE INTO food_tags (tag, label, description) VALUES
    ('alcohol', 'Alcohol', 'beer, wine, cocktails, tiramisu'),
    ('added_sugar', 'Added sugar', 'soda, desserts, sweetened drinks'),
    ('dairy', 'Dairy', 'milk, cheese, cream, butter-heavy dishes'),
    ('gluten', 'Gluten', 'wheat bread, pasta, most baked goods'),
    ('red_meat', 'Red meat', 'beef, lamb, pork'),
    ('processed_meat', 'Processed meat', 'bacon, sausage, deli meats'),
    ('fried', 'Fried', 'deep-fried anything'),
    ('spicy', 'Spicy', 'chili-forward dishes'),
    ('nightshades', 'Nightshades', 'tomato, potato, eggplant, peppers'),
    ('caffeine', 'Caffeine', 'coffee, energy drinks, strong tea'),
    ('shellfish', 'Shellfish', 'shrimp, crab, mussels'),
    ('eggs', 'Eggs', 'eggs and egg-heavy dishes'),
    ('soy', 'Soy', 'tofu, soy sauce, edamame'),
    ('citrus', 'Citrus', 'oranges, lemons, grapefruit');
