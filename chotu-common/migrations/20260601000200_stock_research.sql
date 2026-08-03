-- Create stock_research_log table to log completed stock analyses

CREATE TABLE IF NOT EXISTS stock_research_log (
    id TEXT PRIMARY KEY,               -- UUID
    timestamp DATETIME NOT NULL,       -- Execution time
    tickers_analyzed TEXT NOT NULL,    -- Comma-separated list of ticker symbols
    summary_file_path TEXT NOT NULL    -- Path to the saved Markdown file
);
