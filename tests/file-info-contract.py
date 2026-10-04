#!/usr/bin/env python3
"""Native file metadata follows the existing file boundary without reading bytes."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import importlib.util
import json
import os
from pathlib import Path
import threading
import unittest
from unittest.mock import patch
import urllib.request

spec=importlib.util.spec_from_file_location('file_fixture',Path(__file__).with_name('workspace-contract.py'))
f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)

class FileInfo(unittest.TestCase):
    setUp=f.WorkspaceContract.setUp
    tearDown=f.WorkspaceContract.tearDown
    lead=f.WorkspaceContract.lead
    agent_update=f.WorkspaceContract.agent_update

    def test_large_file_metadata_does_not_read_content(self):
        actor=self.lead();file=self.project/'large.pdf'
        with file.open('wb') as stream:stream.truncate(21*1024*1024)
        with patch.object(Path,'read_bytes',side_effect=AssertionError('Metadata read file bytes')):
            info=self.runtime.file_info(actor['id'],'large.pdf')
        self.assertEqual(info,{'path':str(file.resolve()),'name':'large.pdf','mime':'application/pdf','size':21*1024*1024})
        with self.assertRaisesRegex(ValueError,'preview limit'):
            self.runtime.file_content(actor['id'],'large.pdf')

    def test_paths_assets_and_deleted_owners_match_file_reads(self):
        actor=self.lead();file=self.root/'report.md';file.write_text('# Result\n')
        (self.project/'link.md').symlink_to(file)
        info=self.runtime.file_info(actor['id'],'link.md')
        self.assertEqual(info['path'],str(file.resolve()))
        self.assertEqual(info['size'],len(file.read_bytes()))
        asset=self.runtime.upload_asset({'agent':actor['id'],'name':'note.txt','base64':base64.b64encode(b'Note').decode()})
        self.assertEqual(self.runtime.file_info(asset_id=asset['id'])['name'],'note.txt')
        for agent,path in [('unknown','report.md'),(actor['id'],'missing'),(actor['id'],'.')]:
            with self.assertRaises(ValueError):self.runtime.file_info(agent,path)
        self.agent_update(actor,deletedAt=1)
        with self.assertRaises(ValueError):self.runtime.file_info(asset_id=asset['id'])
        with self.assertRaises(ValueError):self.runtime.file_info(actor['id'],str(file))

    def test_image_workspace_preview_uses_exec_prefix_for_stat_and_read(self):
        actor=self.lead();mount=self.root/'image'/'repo'/'project';mount.mkdir(parents=True)
        file=mount/'preview.txt';file.write_bytes(b'image view')
        log=self.root/'prefix.log';wrapper=self.root/'prefix.sh'
        wrapper.write_text(f'#!/bin/sh\nprintf x >> {str(log)!r}\nshift\nexec "$@"\n')
        wrapper.chmod(0o755)
        self.agent_update(actor,imageWorkspaceReady=True,cwd=str(mount))
        with patch('codex_workspace_images.exec_prefix',return_value=[str(wrapper),'--']):
            info=self.runtime.file_info(actor['id'],'preview.txt')
            content,mime,name=self.runtime.file_content(actor['id'],'preview.txt')
            with self.assertRaisesRegex(ValueError,'preview limit'):
                self.runtime.file_content(actor['id'],'preview.txt',limit=2)
        self.assertEqual(info['size'],len(b'image view'))
        self.assertEqual((content,name),(b'image view','preview.txt'))
        self.assertEqual(log.read_text(),'xxx')

    def test_http_metadata_and_origin_boundary(self):
        from codex_canvas import Canvas,make_server
        canvas=Canvas(self.state);canvas.runtime=self.runtime;server=make_server(canvas)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        try:
            actor=self.lead();(self.project/'report.md').write_text('# Report')
            url=f'http://127.0.0.1:{server.server_port}/api/file-info?agent={actor["id"]}&path=report.md'
            with urllib.request.urlopen(url) as response:value=json.load(response)
            self.assertEqual(value['path'],str((self.project/'report.md').resolve()))
            self.assertNotIn('base64',value)
            with self.assertRaises(urllib.error.HTTPError) as error:
                urllib.request.urlopen(urllib.request.Request(url,headers={'Origin':'https://untrusted.example'}))
            self.assertEqual(error.exception.code,403)
        finally:server.shutdown();server.server_close();thread.join()

if __name__=='__main__':unittest.main()
