# Upgrade guide

1. Refer to the [upgrade notes](https://appflowy.com/docs/self-hosters-upgrade-notes) for any important changes.

2. Sync this fork.

3. Fetch the tags from the upstream by
    ```
    git fetch --tags upstream
    ```
   
4. Merge the new version's tag into the `selfhosted` branch (ideally in an IDE).

5. Push to the origin.

6. On the server, pull form the origin.

7. Update the Docker image version variables in the .env file.

8. Run
    ```
    docker compose up
    ```
   Do not rebuild the compose stack unless you really want to for some reason. It will take a long time. It is preferred
   to pull the prebuilt images.
