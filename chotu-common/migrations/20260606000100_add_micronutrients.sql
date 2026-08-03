-- Migration to add micronutrient tracking to health_family_summary and food_log

-- Add columns to health_family_summary
ALTER TABLE health_family_summary ADD COLUMN omega_3_dha_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN cholesterol_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN saturated_fat_g REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN unsaturated_fat_g REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN triglycerides_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN iron_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN vitamin_b_mg REAL DEFAULT 0.0;
ALTER TABLE health_family_summary ADD COLUMN vitamin_c_mg REAL DEFAULT 0.0;

-- Add columns to food_log
ALTER TABLE food_log ADD COLUMN estimated_omega_3_dha_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_cholesterol_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_saturated_fat_g REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_unsaturated_fat_g REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_triglycerides_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_iron_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_vitamin_b_mg REAL DEFAULT 0.0;
ALTER TABLE food_log ADD COLUMN estimated_vitamin_c_mg REAL DEFAULT 0.0;
