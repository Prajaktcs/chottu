//! Open Food Facts barcode nutrition lookup.
//!
//! API: `GET https://world.openfoodfacts.org/api/v2/product/{barcode}.json`

use serde::Deserialize;

use crate::llm::NutritionEstimation;

const OFF_USER_AGENT: &str = "ChotuPersonalAgent/0.1 (https://github.com/alexexample/chotu)";

#[derive(Debug, Clone)]
pub struct OpenFoodFactsProduct {
    pub barcode: String,
    pub product_name: String,
    pub nutrition: NutritionEstimation,
}

#[derive(Debug, Deserialize)]
struct OffResponse {
    status: i32,
    product: Option<OffProduct>,
}

#[derive(Debug, Deserialize)]
struct OffProduct {
    #[serde(default)]
    product_name: Option<String>,
    #[serde(default)]
    generic_name: Option<String>,
    #[serde(default)]
    brands: Option<String>,
    #[serde(default)]
    serving_quantity: Option<f64>,
    #[serde(default)]
    nutriments: OffNutriments,
}

#[derive(Debug, Default, Deserialize)]
struct OffNutriments {
    #[serde(rename = "energy-kcal_serving")]
    energy_kcal_serving: Option<f64>,
    #[serde(rename = "energy-kcal_100g")]
    energy_kcal_100g: Option<f64>,
    #[serde(rename = "energy-kcal")]
    energy_kcal: Option<f64>,
    #[serde(rename = "proteins_serving")]
    proteins_serving: Option<f64>,
    #[serde(rename = "proteins_100g")]
    proteins_100g: Option<f64>,
    #[serde(rename = "carbohydrates_serving")]
    carbohydrates_serving: Option<f64>,
    #[serde(rename = "carbohydrates_100g")]
    carbohydrates_100g: Option<f64>,
    #[serde(rename = "fat_serving")]
    fat_serving: Option<f64>,
    #[serde(rename = "fat_100g")]
    fat_100g: Option<f64>,
    #[serde(rename = "fiber_serving")]
    fiber_serving: Option<f64>,
    #[serde(rename = "fiber_100g")]
    fiber_100g: Option<f64>,
    #[serde(rename = "sugars_serving")]
    sugars_serving: Option<f64>,
    #[serde(rename = "sugars_100g")]
    sugars_100g: Option<f64>,
    /// Sodium in grams (OFF convention).
    #[serde(rename = "sodium_serving")]
    sodium_serving: Option<f64>,
    #[serde(rename = "sodium_100g")]
    sodium_100g: Option<f64>,
    #[serde(rename = "saturated-fat_serving")]
    saturated_fat_serving: Option<f64>,
    #[serde(rename = "saturated-fat_100g")]
    saturated_fat_100g: Option<f64>,
    #[serde(rename = "trans-fat_serving")]
    trans_fat_serving: Option<f64>,
    #[serde(rename = "trans-fat_100g")]
    trans_fat_100g: Option<f64>,
    /// Cholesterol typically mg when values are large; treat small values as grams.
    #[serde(rename = "cholesterol_serving")]
    cholesterol_serving: Option<f64>,
    #[serde(rename = "cholesterol_100g")]
    cholesterol_100g: Option<f64>,
    /// Minerals below are in grams per OFF docs.
    #[serde(rename = "iron_serving")]
    iron_serving: Option<f64>,
    #[serde(rename = "iron_100g")]
    iron_100g: Option<f64>,
    #[serde(rename = "calcium_serving")]
    calcium_serving: Option<f64>,
    #[serde(rename = "calcium_100g")]
    calcium_100g: Option<f64>,
    #[serde(rename = "potassium_serving")]
    potassium_serving: Option<f64>,
    #[serde(rename = "potassium_100g")]
    potassium_100g: Option<f64>,
    #[serde(rename = "magnesium_serving")]
    magnesium_serving: Option<f64>,
    #[serde(rename = "magnesium_100g")]
    magnesium_100g: Option<f64>,
    #[serde(rename = "zinc_serving")]
    zinc_serving: Option<f64>,
    #[serde(rename = "zinc_100g")]
    zinc_100g: Option<f64>,
    #[serde(rename = "vitamin-c_serving")]
    vitamin_c_serving: Option<f64>,
    #[serde(rename = "vitamin-c_100g")]
    vitamin_c_100g: Option<f64>,
    #[serde(rename = "vitamin-a_serving")]
    vitamin_a_serving: Option<f64>,
    #[serde(rename = "vitamin-a_100g")]
    vitamin_a_100g: Option<f64>,
    #[serde(rename = "vitamin-d_serving")]
    vitamin_d_serving: Option<f64>,
    #[serde(rename = "vitamin-d_100g")]
    vitamin_d_100g: Option<f64>,
    #[serde(rename = "vitamin-e_serving")]
    vitamin_e_serving: Option<f64>,
    #[serde(rename = "vitamin-e_100g")]
    vitamin_e_100g: Option<f64>,
    #[serde(rename = "vitamin-k_serving")]
    vitamin_k_serving: Option<f64>,
    #[serde(rename = "vitamin-k_100g")]
    vitamin_k_100g: Option<f64>,
    #[serde(rename = "caffeine_serving")]
    caffeine_serving: Option<f64>,
    #[serde(rename = "caffeine_100g")]
    caffeine_100g: Option<f64>,
}

/// Look up a product by barcode. Returns `None` when OFF has no product or the request fails softly.
pub async fn lookup_barcode(barcode: &str) -> anyhow::Result<Option<OpenFoodFactsProduct>> {
    let barcode = barcode.trim();
    if barcode.is_empty() || !barcode.chars().all(|c| c.is_ascii_digit()) {
        return Ok(None);
    }

    let url = format!(
        "https://world.openfoodfacts.org/api/v2/product/{}.json",
        barcode
    );
    let client = reqwest::Client::builder()
        .user_agent(OFF_USER_AGENT)
        .build()?;

    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: OffResponse = resp.json().await?;
    if body.status != 1 {
        return Ok(None);
    }
    let product = match body.product {
        Some(p) => p,
        None => return Ok(None),
    };

    let name = product
        .product_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or(product.generic_name.as_deref())
        .unwrap_or("Unknown product")
        .trim()
        .to_string();

    let branded = match product.brands.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(brand) => format!("{} ({})", name, brand),
        None => name,
    };

    let nutrition = map_nutriments(&product.nutriments, product.serving_quantity, &branded);
    Ok(Some(OpenFoodFactsProduct {
        barcode: barcode.to_string(),
        product_name: branded,
        nutrition,
    }))
}

fn pick(serving: Option<f64>, per_100g: Option<f64>, scale: f64) -> f64 {
    if let Some(v) = serving {
        return v.max(0.0);
    }
    per_100g.unwrap_or(0.0).max(0.0) * scale
}

/// Convert OFF mineral grams → mg. Values already looking like mg (>2) are left as-is.
fn grams_to_mg(v: f64) -> f64 {
    if v == 0.0 {
        0.0
    } else if v > 2.0 {
        // Already reported in mg (common for some vitamin-c fields).
        v
    } else {
        v * 1000.0
    }
}

fn cholesterol_to_mg(v: f64) -> f64 {
    if v == 0.0 {
        0.0
    } else if v < 1.0 {
        // Likely grams.
        v * 1000.0
    } else {
        v
    }
}

fn map_nutriments(n: &OffNutriments, serving_quantity: Option<f64>, product_name: &str) -> NutritionEstimation {
    // Prefer serving values; else scale 100g by serving_quantity/100; else assume 100g portion.
    let use_serving = n.energy_kcal_serving.is_some()
        || n.proteins_serving.is_some()
        || n.carbohydrates_serving.is_some()
        || n.fat_serving.is_some();
    let scale = if use_serving {
        1.0
    } else if let Some(sq) = serving_quantity.filter(|q| *q > 0.0) {
        sq / 100.0
    } else {
        1.0
    };

    let calories = if use_serving {
        n.energy_kcal_serving
            .or(n.energy_kcal)
            .unwrap_or(0.0)
            .max(0.0)
    } else {
        n.energy_kcal_100g
            .or(n.energy_kcal)
            .unwrap_or(0.0)
            .max(0.0)
            * scale
    };

    let protein = pick(n.proteins_serving, n.proteins_100g, scale);
    let carbs = pick(n.carbohydrates_serving, n.carbohydrates_100g, scale);
    let fat = pick(n.fat_serving, n.fat_100g, scale);
    let fiber = pick(n.fiber_serving, n.fiber_100g, scale);
    let sugar = pick(n.sugars_serving, n.sugars_100g, scale);
    let sat = pick(n.saturated_fat_serving, n.saturated_fat_100g, scale);
    let trans = pick(n.trans_fat_serving, n.trans_fat_100g, scale);
    let unsat = (fat - sat - trans).max(0.0);

    let sodium_g = pick(n.sodium_serving, n.sodium_100g, scale);
    let iron_g = pick(n.iron_serving, n.iron_100g, scale);
    let calcium_g = pick(n.calcium_serving, n.calcium_100g, scale);
    let potassium_g = pick(n.potassium_serving, n.potassium_100g, scale);
    let magnesium_g = pick(n.magnesium_serving, n.magnesium_100g, scale);
    let zinc_g = pick(n.zinc_serving, n.zinc_100g, scale);
    let chol = pick(n.cholesterol_serving, n.cholesterol_100g, scale);
    let vit_c = pick(n.vitamin_c_serving, n.vitamin_c_100g, scale);
    let vit_a = pick(n.vitamin_a_serving, n.vitamin_a_100g, scale);
    let vit_d = pick(n.vitamin_d_serving, n.vitamin_d_100g, scale);
    let vit_e = pick(n.vitamin_e_serving, n.vitamin_e_100g, scale);
    let vit_k = pick(n.vitamin_k_serving, n.vitamin_k_100g, scale);
    let caffeine = pick(n.caffeine_serving, n.caffeine_100g, scale);

    let dominant_macro = {
        let mut best = ("protein", protein);
        if carbs >= best.1 {
            best = ("carbs", carbs);
        }
        if fat >= best.1 {
            best = ("fat", fat);
        }
        best.0.to_string()
    };

    let portion_note = if use_serving {
        "one serving"
    } else if serving_quantity.is_some() {
        "one labeled serving (scaled from 100g)"
    } else {
        "per 100g (no serving size on file)"
    };

    NutritionEstimation {
        total_calories: calories.round() as i32,
        protein_grams: protein,
        carbs_grams: carbs,
        fats_grams: fat,
        dominant_macro,
        reasoning: format!(
            "Open Food Facts barcode lookup for \"{}\" ({})",
            product_name, portion_note
        ),
        omega_3_dha_mg: 0.0,
        cholesterol_mg: cholesterol_to_mg(chol),
        saturated_fat_g: sat,
        unsaturated_fat_g: unsat,
        triglycerides_mg: 0.0,
        iron_mg: grams_to_mg(iron_g),
        vitamin_b_mg: 0.0,
        vitamin_c_mg: grams_to_mg(vit_c),
        sugar_g: sugar,
        fiber_g: fiber,
        sodium_mg: sodium_g * 1000.0,
        potassium_mg: grams_to_mg(potassium_g),
        calcium_mg: grams_to_mg(calcium_g),
        magnesium_mg: grams_to_mg(magnesium_g),
        zinc_mg: grams_to_mg(zinc_g),
        vitamin_a_mcg: if vit_a > 0.0 && vit_a < 1.0 {
            vit_a * 1_000_000.0
        } else {
            vit_a
        },
        vitamin_d_mcg: if vit_d > 0.0 && vit_d < 0.01 {
            vit_d * 1_000_000.0
        } else {
            vit_d
        },
        vitamin_e_mg: grams_to_mg(vit_e),
        vitamin_k_mcg: if vit_k > 0.0 && vit_k < 0.01 {
            vit_k * 1_000_000.0
        } else {
            vit_k
        },
        caffeine_mg: if caffeine > 0.0 && caffeine < 1.0 {
            caffeine * 1000.0
        } else {
            caffeine
        },
        trans_fat_g: trans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_nutriments_prefers_serving() {
        let n = OffNutriments {
            energy_kcal_serving: Some(200.0),
            energy_kcal_100g: Some(385.0),
            proteins_serving: Some(5.0),
            proteins_100g: Some(9.62),
            carbohydrates_serving: Some(37.0),
            carbohydrates_100g: Some(71.15),
            fat_serving: Some(4.0),
            fat_100g: Some(7.69),
            fiber_serving: Some(1.0),
            sugars_serving: Some(7.0),
            sodium_serving: Some(0.15),
            saturated_fat_serving: Some(1.0),
            ..Default::default()
        };
        let est = map_nutriments(&n, Some(52.0), "Test Noodles");
        assert_eq!(est.total_calories, 200);
        assert!((est.protein_grams - 5.0).abs() < 1e-6);
        assert!((est.sodium_mg - 150.0).abs() < 1e-6);
        assert!(est.reasoning.contains("Open Food Facts"));
    }

    #[test]
    fn test_map_nutriments_scales_100g_by_serving_quantity() {
        let n = OffNutriments {
            energy_kcal_100g: Some(200.0),
            proteins_100g: Some(10.0),
            carbohydrates_100g: Some(20.0),
            fat_100g: Some(5.0),
            sodium_100g: Some(0.1),
            ..Default::default()
        };
        // 50g serving → half of 100g values
        let est = map_nutriments(&n, Some(50.0), "Snack");
        assert_eq!(est.total_calories, 100);
        assert!((est.protein_grams - 5.0).abs() < 1e-6);
        assert!((est.sodium_mg - 50.0).abs() < 1e-6);
    }
}
