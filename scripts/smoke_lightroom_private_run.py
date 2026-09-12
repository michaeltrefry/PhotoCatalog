#!/usr/bin/env python3
"""Exercise the real frozen inspection CLI using only newly created synthetic files."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import sqlite3
import time

import run_lightroom_inspection as driver


def source_digests(root):
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(root.rglob("*")) if path.is_file()}


def fixture(path, photo_root, touch):
    packet = '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="5"/></rdf:RDF></x:xmpmeta>'
    with sqlite3.connect(path) as db:
        db.executescript('''
CREATE TABLE Adobe_variablesTable(id_local INTEGER PRIMARY KEY,name TEXT,value TEXT);
INSERT INTO Adobe_variablesTable VALUES(1,'Adobe_storeProviderID','synthetic-runner-provider'),(2,'Adobe_DBVersion','1300000');
CREATE TABLE AgLibraryRootFolder(id_local INTEGER PRIMARY KEY,id_global TEXT,absolutePath TEXT);
CREATE TABLE AgLibraryFolder(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFolder INTEGER,pathFromRoot TEXT);
INSERT INTO AgLibraryFolder VALUES(1,'folder',1,'2022/');
CREATE TABLE AgLibraryFile(id_local INTEGER PRIMARY KEY,id_global TEXT,folder INTEGER,idx_filename TEXT);
INSERT INTO AgLibraryFile VALUES(1,'file',1,'missing.CR2');
CREATE TABLE Adobe_images(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFile INTEGER,masterImage INTEGER,copyName TEXT,touchTime REAL);
CREATE TABLE Adobe_AdditionalMetadata(id_local INTEGER PRIMARY KEY,image INTEGER,xmp TEXT);
CREATE TABLE Adobe_imageDevelopSettings(id_local INTEGER PRIMARY KEY,image INTEGER,hasBigData INTEGER,text TEXT);
INSERT INTO Adobe_imageDevelopSettings VALUES(1,1,1,'opaque Adobe instructions; never evaluate');
CREATE TABLE Adobe_libraryImageDevelopHistoryStep(id_local INTEGER PRIMARY KEY,image INTEGER,dateCreated REAL,text TEXT);
INSERT INTO Adobe_libraryImageDevelopHistoryStep VALUES(1,1,1,'history retained only');
CREATE TABLE Adobe_libraryImageDevelopSnapshot(id_local INTEGER PRIMARY KEY,image INTEGER,dateCreated REAL,text TEXT);
INSERT INTO Adobe_libraryImageDevelopSnapshot VALUES(1,1,1,'snapshot retained only');
CREATE TABLE Opaque(k INTEGER PRIMARY KEY,v TEXT,b BLOB);
INSERT INTO Opaque VALUES(1,CAST(x'fffe00' AS TEXT),x'00ff'),(2,'retained row 2',x'10');
''')
        db.execute("INSERT INTO AgLibraryRootFolder VALUES(1,'root',?)", (str(photo_root),))
        db.execute("INSERT INTO Adobe_images VALUES(1,'master',1,NULL,NULL,?),(2,'copy',1,1,'virtual copy',?)", (touch, touch))
        db.execute("INSERT INTO Adobe_AdditionalMetadata VALUES(1,1,?)", (packet,))
    auxiliary = path.with_suffix('.lrcat-data')
    auxiliary.mkdir()
    (auxiliary/'opaque.dat').write_bytes(b'synthetic opaque auxiliary bytes')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    root = args.output.absolute()
    root.mkdir(mode=0o700)
    sources = root/'synthetic-sources'
    catalogs = sources/'catalogs'
    photos = sources/'photos'
    catalogs.mkdir(parents=True)
    (photos/'2022').mkdir(parents=True)
    fixture(catalogs/'2022-v13.lrcat', photos, 10)
    fixture(catalogs/'2022-v13-3.lrcat', photos, 20)
    (photos/'2022/missing.xmp').write_text('<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="1"/></rdf:RDF></x:xmpmeta>')
    before = source_digests(sources)
    config = copy.deepcopy(driver.DEFAULTS)
    config.update(tested_binary=str(args.binary.absolute()), catalog_root=str(catalogs), original_root=str(photos),
                  exclusive_output=str(root/'run'), minimum_free_bytes=driver.MIB, page_limit=2, resume_rows_per_call=2)
    config_path = root/'config.json'
    config_path.write_text(json.dumps(config, indent=2)+'\n')
    outcome = {"started_unix": time.time(), "core_source": driver.SOURCE, "driver_sha256": driver.file_sha(driver.__file__),
               "smoke_sha256": driver.file_sha(__file__), "binary_sha256": driver.file_sha(args.binary),
               "scope": "disposable synthetic SQLite/auxiliary/XMP only; no RAID paths or pixel decoding", "source_digests_before": before}
    runner = None
    try:
        runner = driver.Runner(driver.initialize(config_path))
        main_review = runner.main_phase()
        main_value = driver.read_json(main_review)
        assert main_value['candidate_count'] == 2
        assert main_value['outcome_counts'] == {'inspected_with_reported_limits': 2}, main_value['outcomes']
        assert len(main_value['families']['families']) == 1
        assert main_value['families']['families'][0]['selected'] is None
        full_review = runner.full_phase(runner.root/'reports/full-capture-request-template.json')
        full_value = driver.read_json(full_review)
        assert full_value['outcome_counts']['full_requested'] == 1
        assert full_value['outcome_counts']['full_capture_failures'] == 0
        assert full_value['outcome_counts']['inspection_failures'] == 0, full_value['outcomes']
        paths_review = runner.path_phase(full_review, False)
        paths_value = driver.read_json(paths_review)
        assert paths_value['outcome_counts']['failures'] == 0
        assert paths_value['outcomes'][0]['pages']['paths']['counts']['missing'] == 1
        packets_review = runner.path_phase(full_review, True)
        packets_value = driver.read_json(packets_review)
        assert packets_value['outcome_counts']['failures'] == 0, packets_value['outcomes']
        assert packets_value['outcomes'][0]['pages']['packets']['rows'] >= 3
        assert packets_value['outcomes'][0]['pages']['metadata-conflicts']['rows'] >= 2
        assert all(family['selected'] is None for family in packets_value['families']['families'])
        captures_before = sorted(path.name for path in (runner.root/'captures').iterdir())
        runner.close()
        runner = driver.Runner(root/'run')
        assert runner.main_phase() == main_review
        assert runner.full_phase(runner.root/'reports/full-capture-request-template.json') == full_review
        assert runner.path_phase(full_review, False) == paths_review
        assert runner.path_phase(full_review, True) == packets_review
        assert sorted(path.name for path in (runner.root/'captures').iterdir()) == captures_before
        assert source_digests(sources) == before
        outcome.update(all_pass=True, reviews={str(path.name): driver.file_sha(path) for path in [main_review, full_review, paths_review, packets_review]},
                       capture_directories=len(captures_before), replay_created_no_capture=True, source_digests_after=source_digests(sources))
    except BaseException as error:
        outcome.update(all_pass=False, error=repr(error))
        raise
    finally:
        if runner:
            runner.close()
        outcome['finished_unix'] = time.time()
        driver.durable_json(root/'smoke-receipt.json', outcome)
        print(json.dumps(outcome, indent=2))


if __name__ == '__main__':
    main()
