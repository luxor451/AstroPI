use std::fs::File;
use std::io::{BufRead, BufReader};

/// Messier object with coordinates
#[derive(Debug, Clone)]
pub struct MessierObject {
    pub name: String,
    pub number: u32,
    pub ra: String,
    pub dec: String,
}

/// Load Messier catalogue from CSV file (sorted by number for binary search)
pub fn load_messier_catalogue(path: &str) -> Result<Vec<MessierObject>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut objects = Vec::new();

    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if i == 0 || line.trim().is_empty() {
            continue; // Skip header
        }

        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() >= 3 {
            let name = fields[0].trim().to_string();
            let ra = fields[1].trim().to_string();
            let dec = fields[2].trim().to_string();

            // Extract Messier number
            if let Some(num) = extract_messier_number(&name) {
                // Skip objects with missing coordinates
                if !ra.is_empty() && !dec.is_empty() {
                    objects.push(MessierObject {
                        name,
                        number: num,
                        ra,
                        dec,
                    });
                }
            }
        }
    }

    // Sort by Messier number for binary search
    objects.sort_by_key(|o| o.number);
    Ok(objects)
}

/// Extract Messier number from name (e.g., "M101" -> 101)
pub fn extract_messier_number(name: &str) -> Option<u32> {
    let name_upper = name.to_uppercase();
    if let Some(pos) = name_upper.find('M') {
        let after_m = &name_upper[pos + 1..];
        let num_str: String = after_m
            .chars()
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !num_str.is_empty() {
            return num_str.parse().ok();
        }
    }
    None
}

/// Binary search for a Messier object by number
pub fn find_messier_object(catalogue: &[MessierObject], number: u32) -> Option<&MessierObject> {
    catalogue
        .binary_search_by_key(&number, |o| o.number)
        .ok()
        .map(|idx| &catalogue[idx])
}
