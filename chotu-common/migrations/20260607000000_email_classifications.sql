-- Migration: Add tables for new email classifications (tasks, travel_itineraries, upcoming_bills, personal_references)

CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    timestamp DATETIME NOT NULL,
    task_description TEXT NOT NULL,
    status TEXT NOT NULL          -- 'pending', 'completed'
);

CREATE TABLE IF NOT EXISTS travel_itineraries (
    id TEXT PRIMARY KEY,
    timestamp DATETIME NOT NULL,
    destination TEXT NOT NULL,
    start_date TEXT,
    end_date TEXT,
    details TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS upcoming_bills (
    id TEXT PRIMARY KEY,
    timestamp DATETIME NOT NULL,
    biller TEXT NOT NULL,
    amount REAL,
    due_date TEXT,
    status TEXT NOT NULL          -- 'unpaid', 'paid'
);

CREATE TABLE IF NOT EXISTS personal_references (
    id TEXT PRIMARY KEY,
    timestamp DATETIME NOT NULL,
    title TEXT NOT NULL,
    url TEXT,
    notes TEXT NOT NULL
);
