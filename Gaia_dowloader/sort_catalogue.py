import pandas as pd
import os

INPUT_FILE = "gaia_local.csv"
OUTPUT_FILE = "gaia_sorted.csv"

# 1. Check if file exists
if not os.path.exists(INPUT_FILE):
    print(f"Error: {INPUT_FILE} not found.")
    exit()

print(f"Loading {INPUT_FILE} into memory... (This requires ~1-2GB RAM)")

# 2. Load the CSV
# We assume the file has a header. If not, remove 'header=0' and add 'names=[...]'
try:
    df = pd.read_csv(INPUT_FILE, header=0, dtype={
        'ra': float, 
        'dec': float, 
        'phot_g_mean_mag': float,
    })
except ValueError:
    # Fallback if headers are missing or messy
    print("Warning: Issue reading header, attempting to read without header...")
    df = pd.read_csv(INPUT_FILE, names=["ra","dec","phot_g_mean_mag"])

print(f"Loaded {len(df)} rows. Sorting...")

# 3. Sort the Data
# Ascending=True means:
# RA: 0 to 360
# Dec: -90 to +90
# Mag: Lower number (Brighter) first
df_sorted = df.sort_values(
    by=['ra', 'dec', 'phot_g_mean_mag'], 
    ascending=[True, True, True]
)

# 4. Save to new file
print(f"Saving sorted data to {OUTPUT_FILE}...")
df_sorted.to_csv(OUTPUT_FILE, index=False)

print("Done! Your catalog is now ordered.")