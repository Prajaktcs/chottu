-- Track Google Health nutrition-log data point IDs for two-way /food sync.
ALTER TABLE food_log ADD COLUMN google_data_point_id TEXT;
