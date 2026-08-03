-- Migration to add remaining standard macro- and micro-nutrients

-- Add columns to health_family_summary
ALTER TABLE health_family_summary ADD COLUMN sugar_g REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN fiber_g REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN sodium_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN potassium_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN calcium_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN magnesium_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN zinc_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN vitamin_a_mcg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN vitamin_d_mcg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN vitamin_e_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN vitamin_k_mcg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN caffeine_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN trans_fat_g REAL DEFAULT 0.0;

-- Add columns to food_log
ALTER TABLE food_log ADD COLUMN estimated_sugar_g REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_fiber_g REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_sodium_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_potassium_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_calcium_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_magnesium_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_zinc_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_vitamin_a_mcg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_vitamin_d_mcg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_vitamin_e_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_vitamin_k_mcg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_caffeine_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_trans_fat_g REAL DEFAULT 0.0;
