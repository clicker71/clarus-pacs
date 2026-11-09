# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

import os

from pydicom import dcmread
from pydicom.dataset import Dataset
from pynetdicom import AE
from pynetdicom.sop_class import (
    ComputedRadiographyImageStorage,
    PatientRootQueryRetrieveInformationModelFind,
)

# C-STORE
ae = AE()
ae.add_requested_context(ComputedRadiographyImageStorage)
files = [f for f in os.listdir('test-images') if f.endswith('.dcm')]
if files:
    assoc = ae.associate('127.0.0.1', 11112, ae_title='TEST_BRIDGE')
    if assoc.is_established:
        ds = dcmread(os.path.join('test-images', files[0]))
        status = assoc.send_c_store(ds)
        print(f'C-STORE: status=0x{status.Status:04X}')
        assoc.release()
    else:
        print('C-STORE: FAILED to associate')

# C-FIND after upload
ae2 = AE()
ae2.add_requested_context(PatientRootQueryRetrieveInformationModelFind)
assoc2 = ae2.associate('127.0.0.1', 11112, ae_title='TEST_BRIDGE')
if assoc2.is_established:
    ds2 = Dataset()
    ds2.PatientName = '*'
    ds2.QueryRetrieveLevel = 'PATIENT'
    responses = assoc2.send_c_find(ds2, PatientRootQueryRetrieveInformationModelFind)
    count = 0
    for status, identifier in responses:
        if status and identifier:
            count += 1
            name = str(getattr(identifier, 'PatientName', '?'))
            print(f'C-FIND: PatientName={name}')
        elif status:
            print(f'C-FIND: final status=0x{status.Status:04X}')
    print(f'C-FIND: found {count} patient(s)')
    assoc2.release()

# C-MOVE test (should be rejected)
ae3 = AE()
from pynetdicom.sop_class import StudyRootQueryRetrieveInformationModelMove

ae3.add_requested_context(StudyRootQueryRetrieveInformationModelMove)
assoc3 = ae3.associate('127.0.0.1', 11112, ae_title='TEST_BRIDGE')
if assoc3.is_established:
    ds3 = Dataset()
    ds3.QueryRetrieveLevel = 'STUDY'
    ds3.StudyInstanceUID = '1.2.3'
    responses = assoc3.send_c_move(ds3, b'TEST_DEST', StudyRootQueryRetrieveInformationModelMove)
    for status, identifier in responses:
        print(f'C-MOVE: status=0x{status.Status:04X}')
    assoc3.release()

print('All tests done.')
