import pandas as pd
from astroquery.gaia import Gaia
import time
import warnings
import os

# CONFIGURATION
OUTPUT_FILE = "gaia_local.csv"
MAG_LIMIT = 14.0
CHUNK_SIZE = 10  # 5 Degree slices (Small enough to never hit the 3M limit)

# Setup
warnings.filterwarnings("ignore")
Gaia.ROW_LIMIT = -1

# Create the file and write the header
if not os.path.exists(OUTPUT_FILE):
    with open(OUTPUT_FILE, 'w') as f:
        f.write("ra,dec,phot_g_mean_mag\n")

print(f"--- Starting ROBUST Download (Mag < {MAG_LIMIT}) ---")
print(f"Output: {OUTPUT_FILE}")

# Loop through the sky in 10-degree slices
for ra_start in range(0, 360, CHUNK_SIZE):
    ra_end = ra_start + CHUNK_SIZE
    
    print(f"\nProcessing Chunk: RA {ra_start}° to {ra_end}°...")

    # 1. Define Query
    # We use 'launch_job_async' to avoid the 2000 row timeout
    query = f"""
    SELECT ra, dec, phot_g_mean_mag
    FROM gaiadr3.gaia_source
    WHERE phot_g_mean_mag < {MAG_LIMIT}
    AND ra >= {ra_start} AND ra < {ra_end}
    """

    # 2. Launch Async Job
    try:
        # background=False makes it wait for the job to finish (Sync-like behavior but Robust)
        job = Gaia.launch_job_async(query, background=False)
        results = job.get_results()
        
        count = len(results)
        print(f"  -> Downloaded {count} stars.")

        # 3. Save to Disk
        if count > 0:
            df = results.to_pandas()
            # Append without header
            df.to_csv(OUTPUT_FILE, mode='a', header=False, index=False)
            
        # 4. Clean up (Delete job from server to save quota)
        # Note: Anonymous jobs auto-delete, but this is good practice
        try:
            Gaia.remove_jobs([job.jobid])
        except:
            pass

    except Exception as e:
        print(f"  -> [ERROR] Failed chunk {ra_start}: {e}")
        time.sleep(5) # Wait and proceed

print(f"\nSUCCESS! All chunks merged into {OUTPUT_FILE}")