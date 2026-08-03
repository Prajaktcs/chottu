-- Migration to add estimated macros to the food_log table
ALTER TABLE food_log ADD COLUMN estimated_protein REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_carbs REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_fats REAL DEFAULT 0.0;
